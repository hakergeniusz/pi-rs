//! `ScopedModelsSelectorComponent` — the `/scoped-models` dialog.
//!
//! Ground truth: `modes/interactive/components/scoped-models-selector.ts`
//! (1.5.2, 412 lines), ported in full: the enabled-id algebra
//! (`isEnabled`/`normalizeEnabled`/`toggle`/`enableAll`/`clearAll`/`move`/
//! `getSortedIds`, `scoped-models-selector.ts:16-78`), the list rendering
//! (`updateList`, `:246-312`), and the key handling (`handleInput`,
//! `:314-411`). Changes are session-only until explicitly persisted with
//! `app.models.save` (ctrl+s) — `onChange` fires for every session-visible
//! change, `onPersist` for the settings write, `onCancel` for escape/ctrl+c.
//!
//! Mechanism notes (upstream shapes → port shapes): the TS `Container`
//! tree renders as the flat line vector every port dialog uses; the
//! callbacks object becomes the event queue the orchestrator drains
//! ([`ScopedModelsSelectorEvent`]); `EnabledIds` (`string[] | null`) is
//! `Option<Vec<String>>` where `None` means "all enabled, no filter".

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::input::{Input, InputOptions};
use pi_tui::theme::ThemeColor;

use super::model_selector::{get_distinct_model_name, get_select_page_index};
use crate::modes::interactive::theme;

/// The window of models shown at once (`maxVisible`,
/// `scoped-models-selector.ts:103`).
const MAX_VISIBLE: usize = 8;

/// Events the selector publishes to the orchestrator — the
/// `ModelsCallbacks` trio (`scoped-models-selector.ts:118-131`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopedModelsSelectorEvent {
    /// `onChange` — the enabled set or its order changed (session-only).
    Change(Option<Vec<String>>),
    /// `onPersist` — persist the current selection to settings.
    Persist(Option<Vec<String>>),
    /// `onCancel` — escape, or ctrl+c with an empty search.
    Cancel,
}

/// The refresh-status kinds (`setRefreshStatus`'s `kind`,
/// `scoped-models-selector.ts:185-187`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStatusKind {
    Muted,
    Success,
    Warning,
}

/// The constructor config (`ModelsConfig`, `scoped-models-selector.ts:106-111`).
#[derive(Debug, Clone, Default)]
pub struct ModelsConfig {
    pub all_models: Vec<pi_ai::types::Model>,
    /// `null` = all enabled (no filter), list = explicit ordered selection.
    pub enabled_model_ids: Option<Vec<String>>,
    /// The initial refresh status line, when a refresh is in flight.
    pub refresh_status: Option<String>,
}

/// `EnabledIds`: `None` = all enabled (no filter), list = explicit ordered
/// list (`EnabledIds`, `scoped-models-selector.ts:15`).
pub type EnabledIds = Option<Vec<String>>;

/// `isEnabled` (`scoped-models-selector.ts:17-19`).
fn is_enabled(enabled_ids: &EnabledIds, id: &str) -> bool {
    match enabled_ids {
        None => true,
        Some(ids) => ids.iter().any(|existing| existing == id),
    }
}

/// `normalizeEnabled` (`scoped-models-selector.ts:22-24`): collapse an
/// explicit list back to null (= all enabled) when it covers every
/// available model.
fn normalize_enabled(result: Vec<String>, all_ids: &[String]) -> EnabledIds {
    if result.len() == all_ids.len() && result.iter().all(|id| all_ids.contains(id)) {
        None
    } else {
        Some(result)
    }
}

/// `toggle` (`scoped-models-selector.ts:26-33`).
fn toggle(enabled_ids: &EnabledIds, all_ids: &[String], id: &str) -> EnabledIds {
    let Some(ids) = enabled_ids else {
        return Some(
            all_ids
                .iter()
                .filter(|model_id| model_id.as_str() != id)
                .cloned()
                .collect(),
        );
    };
    let Some(index) = ids.iter().position(|existing| existing == id) else {
        let mut next = ids.clone();
        next.push(id.to_string());
        return normalize_enabled(next, all_ids);
    };
    let mut next = ids.clone();
    next.remove(index);
    Some(next)
}

/// `enableAll` (`scoped-models-selector.ts:35-45`).
fn enable_all(
    enabled_ids: &EnabledIds,
    all_ids: &[String],
    target_ids: Option<&[String]>,
) -> EnabledIds {
    // Already all enabled (`scoped-models-selector.ts:37-38`).
    let Some(ids) = enabled_ids else {
        return None;
    };
    let targets: &[String] = target_ids.unwrap_or(all_ids);
    let mut result = ids.clone();
    for id in targets {
        if !result.contains(id) {
            result.push(id.clone());
        }
    }
    normalize_enabled(result, all_ids)
}

/// `clearAll` (`scoped-models-selector.ts:47-56`).
// Upstream's clearAll always returns a list; the `Option` wrap keeps the
// return in the `EnabledIds` union.
#[allow(clippy::unnecessary_wraps)]
fn clear_all(
    enabled_ids: &EnabledIds,
    all_ids: &[String],
    target_ids: Option<&[String]>,
) -> EnabledIds {
    match enabled_ids {
        None => Some(match target_ids {
            Some(targets) => all_ids
                .iter()
                .filter(|id| !targets.contains(id))
                .cloned()
                .collect(),
            None => Vec::new(),
        }),
        Some(ids) => {
            let targets: Vec<&String> = match target_ids {
                Some(targets) => targets.iter().collect(),
                None => ids.iter().collect(),
            };
            Some(
                ids.iter()
                    .filter(|id| !targets.contains(id))
                    .cloned()
                    .collect(),
            )
        }
    }
}

/// `move` (`scoped-models-selector.ts:58-71`): swap the id with its
/// neighbor at `delta`.
fn move_enabled(enabled_ids: &EnabledIds, id: &str, delta: isize) -> EnabledIds {
    let Some(ids) = enabled_ids else {
        return None;
    };
    let mut list = ids.clone();
    let Some(index) = list.iter().position(|existing| existing == id) else {
        return Some(list);
    };
    let new_index = index as isize + delta;
    if new_index < 0 || new_index >= list.len() as isize {
        return Some(list);
    }
    list.swap(index, new_index as usize);
    Some(list)
}

/// `getSortedIds` (`scoped-models-selector.ts:73-78`): the explicit order
/// first, then the unlisted models in catalog order.
fn get_sorted_ids(enabled_ids: &EnabledIds, all_ids: &[String]) -> Vec<String> {
    match enabled_ids {
        None => all_ids.to_vec(),
        Some(ids) => {
            let mut sorted = ids.clone();
            for id in all_ids {
                if !sorted.contains(id) {
                    sorted.push(id.clone());
                }
            }
            sorted
        }
    }
}

/// One rendered list row's data (`ModelItem`,
/// `scoped-models-selector.ts:80-84`).
#[derive(Clone)]
struct ModelItem {
    full_id: String,
    model: Option<pi_ai::types::Model>,
    enabled: bool,
}

/// `getModelSearchText` (`model-search.ts:7-12`): the search haystack the
/// list filters over.
fn get_model_search_text(id: &str, provider: &str, name: &str) -> String {
    let name = if name.is_empty() {
        String::new()
    } else {
        format!(" {name}")
    };
    format!("{id} {provider} {provider}/{id} {provider} {id}{name}")
}

/// The selector dialog (`ScopedModelsSelectorComponent`,
/// `scoped-models-selector.ts:133-412`). Field names mirror upstream's.
#[allow(clippy::struct_field_names)] // mirrors the TS component's fields; split when extending
pub struct ScopedModelsSelectorComponent {
    models_by_id: BTreeMap<String, pi_ai::types::Model>,
    all_ids: Vec<String>,
    enabled_ids: EnabledIds,
    filtered_items: Vec<ModelItem>,
    selected_index: usize,
    search_input: Input,
    /// The `listContainer`'s rows, rebuilt by [`Self::update_list`].
    list_lines: Vec<String>,
    is_dirty: bool,
    /// The live refresh status: `(message, kind)` (`refreshStatusText`).
    refresh_status: Option<(String, RefreshStatusKind)>,
    events: Arc<Mutex<Vec<ScopedModelsSelectorEvent>>>,
}

impl ScopedModelsSelectorComponent {
    /// The constructor (`scoped-models-selector.ts:106-155`).
    pub fn new(config: ModelsConfig, events: Arc<Mutex<Vec<ScopedModelsSelectorEvent>>>) -> Self {
        let mut selector = ScopedModelsSelectorComponent {
            models_by_id: BTreeMap::new(),
            all_ids: Vec::new(),
            enabled_ids: config.enabled_model_ids.clone(),
            filtered_items: Vec::new(),
            selected_index: 0,
            search_input: Input::new(InputOptions::default()),
            list_lines: Vec::new(),
            is_dirty: false,
            refresh_status: config
                .refresh_status
                .map(|text| (text, RefreshStatusKind::Muted)),
            events,
        };
        for model in &config.all_models {
            let full_id = format!("{}/{}", model.provider, model.id);
            selector.models_by_id.insert(full_id.clone(), model.clone());
            selector.all_ids.push(full_id);
        }
        selector.filtered_items = selector.build_items();
        selector.update_list();
        selector
    }

    /// Take the queued events (the orchestrator drains after each pump).
    pub fn take_events(&mut self) -> Vec<ScopedModelsSelectorEvent> {
        std::mem::take(self.events.lock().expect("selector events").as_mut())
    }

    /// `updateModels` (`scoped-models-selector.ts:157-183`): swap the model
    /// list (and optionally the enabled set) while keeping the selected
    /// entry selected. `None` leaves the enabled set alone (upstream's
    /// `undefined`), `Some(None)` is upstream's `null` (all enabled).
    pub fn update_models(
        &mut self,
        models: &[pi_ai::types::Model],
        enabled_model_ids: Option<EnabledIds>,
    ) {
        let selected_id = self
            .filtered_items
            .get(self.selected_index)
            .map(|item| item.full_id.clone());
        if let Some(enabled_model_ids) = enabled_model_ids {
            self.enabled_ids = enabled_model_ids;
        }
        self.models_by_id.clear();
        self.all_ids.clear();
        for model in models {
            let full_id = format!("{}/{}", model.provider, model.id);
            self.models_by_id.insert(full_id.clone(), model.clone());
            self.all_ids.push(full_id);
        }
        self.refresh();
        let refreshed_index = selected_id.as_ref().and_then(|selected| {
            self.filtered_items
                .iter()
                .position(|item| &item.full_id == selected)
        });
        if let Some(index) = refreshed_index {
            self.selected_index = index;
            self.update_list();
        }
    }

    /// `setRefreshStatus` (`scoped-models-selector.ts:185-187`).
    pub fn set_refresh_status(&mut self, message: &str, kind: RefreshStatusKind) {
        self.refresh_status = Some((message.to_string(), kind));
    }

    /// `buildItems` (`scoped-models-selector.ts:189-196`).
    fn build_items(&self) -> Vec<ModelItem> {
        get_sorted_ids(&self.enabled_ids, &self.all_ids)
            .into_iter()
            .map(|id| {
                let model = self.models_by_id.get(&id).cloned();
                let enabled = is_enabled(&self.enabled_ids, &id);
                ModelItem {
                    full_id: id,
                    model,
                    enabled,
                }
            })
            .collect()
    }

    /// `getFooterText` (`scoped-models-selector.ts:198-221`): the hint row
    /// with the enabled counts; `(unsaved)` when dirty.
    fn footer_text(&self) -> String {
        let theme = theme::theme();
        let enabled_count = match &self.enabled_ids {
            Some(ids) => ids
                .iter()
                .filter(|id| self.models_by_id.contains_key(*id))
                .count(),
            None => self.all_ids.len(),
        };
        let unavailable_count = match &self.enabled_ids {
            Some(ids) => ids
                .iter()
                .filter(|id| !self.models_by_id.contains_key(*id))
                .count(),
            None => 0,
        };
        let count_text = if self.enabled_ids.is_none() {
            "all enabled".to_string()
        } else if unavailable_count > 0 {
            format!("{enabled_count}/{} enabled · {unavailable_count} unavailable", self.all_ids.len())
        } else {
            format!("{enabled_count}/{} enabled", self.all_ids.len())
        };
        let parts = [
            format!("{} toggle", theme::key_display_text("tui.select.confirm")),
            format!(
                "{} all",
                theme::key_display_text(theme::app_keys::MODELS_ENABLE_ALL)
            ),
            format!(
                "{} clear",
                theme::key_display_text(theme::app_keys::MODELS_CLEAR_ALL)
            ),
            format!(
                "{} provider",
                theme::key_display_text(theme::app_keys::MODELS_TOGGLE_PROVIDER)
            ),
            format!(
                "{}/{} reorder",
                theme::key_display_text(theme::app_keys::MODELS_REORDER_UP),
                theme::key_display_text(theme::app_keys::MODELS_REORDER_DOWN)
            ),
            format!(
                "{} save",
                theme::key_display_text(theme::app_keys::MODELS_SAVE)
            ),
            count_text,
        ];
        let joined = parts.join(" · ");
        if self.is_dirty {
            // `theme.fg("dim", `  ${parts.join(" · ")} `) +
            // theme.fg("warning", "(unsaved)")`.
            format!(
                "{}{}",
                theme.fg(ThemeColor::Dim, &format!("  {joined} ")),
                theme.fg(ThemeColor::Warning, "(unsaved)")
            )
        } else {
            theme.fg(ThemeColor::Dim, &format!("  {joined}"))
        }
    }

    /// `refresh` (`scoped-models-selector.ts:223-240`).
    fn refresh(&mut self) {
        let query = self.search_input.value().to_string();
        let items = self.build_items();
        self.filtered_items = if query.is_empty() {
            items
        } else {
            pi_tui::fuzzy::fuzzy_filter(&items, &query, |item| match &item.model {
                Some(model) => get_model_search_text(&model.id, &model.provider, &model.name),
                None => item.full_id.clone(),
            })
        };
        self.selected_index = self
            .selected_index
            .min(self.filtered_items.len().saturating_sub(1));
        self.update_list();
    }

    /// `notifyChange` (`scoped-models-selector.ts:242-244`).
    fn notify_change(&self) {
        self.events
            .lock()
            .expect("selector events")
            .push(ScopedModelsSelectorEvent::Change(self.enabled_ids.clone()));
    }

    /// `updateList` (`scoped-models-selector.ts:246-312`).
    #[allow(clippy::too_many_lines)] // mirrors the TS updateList; split when extending
    fn update_list(&mut self) {
        let theme = theme::theme();
        self.list_lines.clear();

        if self.filtered_items.is_empty() {
            self.list_lines
                .push(theme.fg(ThemeColor::Muted, "  No matching models"));
            return;
        }

        let start = self
            .selected_index
            .saturating_sub(MAX_VISIBLE / 2)
            .min(self.filtered_items.len().saturating_sub(MAX_VISIBLE));
        let end = (start + MAX_VISIBLE).min(self.filtered_items.len());
        for index in start..end {
            let item = &self.filtered_items[index];
            let is_selected = index == self.selected_index;
            let prefix = if is_selected {
                theme.fg(ThemeColor::Accent, "→ ")
            } else {
                "  ".to_string()
            };
            let id = item
                .model
                .as_ref()
                .map_or_else(|| item.full_id.clone(), |model| model.id.clone());
            let styled_id = if item.model.is_some() {
                id
            } else {
                theme::strikethrough(&id)
            };
            let model_text = if is_selected {
                theme.fg(ThemeColor::Accent, &styled_id)
            } else {
                styled_id
            };
            let name = item
                .model
                .as_ref()
                .and_then(|model| get_distinct_model_name(&model.id, &model.name));
            let name_text = name
                .map(|name| theme.fg(ThemeColor::Muted, &format!(" ({name})")))
                .unwrap_or_default();
            let provider_badge = match &item.model {
                Some(model) => theme.fg(ThemeColor::Muted, &format!(" [{}]", model.provider)),
                None => theme.fg(ThemeColor::Muted, " [unavailable]"),
            };
            let status = if item.model.is_some() && item.enabled {
                theme.fg(ThemeColor::Accent, "✓ ")
            } else {
                "  ".to_string()
            };
            self.list_lines
                .push(format!("{prefix}{status}{model_text}{name_text}{provider_badge}"));
        }

        // Add scroll indicator if needed
        if start > 0 || end < self.filtered_items.len() {
            self.list_lines.push(theme.fg(
                ThemeColor::Muted,
                &format!("  ({}/{})", self.selected_index + 1, self.filtered_items.len()),
            ));
        }

        let selected = &self.filtered_items[self.selected_index];
        self.list_lines.push(String::new());
        let detail = match &selected.model {
            Some(model) => format!("  Model Name: {}", model.name),
            None => "  Model unavailable".to_string(),
        };
        self.list_lines.push(theme.fg(ThemeColor::Muted, &detail));
    }

    /// The filtered-list target ids for enable/clear-all (the
    /// `filteredItems.map((i) => i.fullId)` argument upstream passes only
    /// when a search is active, `scoped-models-selector.ts:382`/`:393`).
    fn filtered_target_ids(&self) -> Option<Vec<String>> {
        if self.search_input.value().is_empty() {
            None
        } else {
            Some(
                self.filtered_items
                    .iter()
                    .map(|item| item.full_id.clone())
                    .collect(),
            )
        }
    }

    /// `handleInput` (`scoped-models-selector.ts:314-411`).
    #[allow(clippy::too_many_lines)] // mirrors the TS handleInput; split when extending
    fn handle_selector_input(&mut self, data: &str) {
        // Navigation
        if pi_tui::keybindings::matches(data, pi_tui::keybindings::action::SELECT_UP) {
            if self.filtered_items.is_empty() {
                return;
            }
            self.selected_index = if self.selected_index == 0 {
                self.filtered_items.len() - 1
            } else {
                self.selected_index - 1
            };
            self.update_list();
            return;
        }
        if pi_tui::keybindings::matches(data, pi_tui::keybindings::action::SELECT_DOWN) {
            if self.filtered_items.is_empty() {
                return;
            }
            self.selected_index = if self.selected_index == self.filtered_items.len() - 1 {
                0
            } else {
                self.selected_index + 1
            };
            self.update_list();
            return;
        }
        if let Some(page_index) = get_select_page_index(
            data,
            self.selected_index,
            self.filtered_items.len(),
            MAX_VISIBLE,
        ) {
            if self.filtered_items.is_empty() {
                return;
            }
            self.selected_index = page_index;
            self.update_list();
            return;
        }

        // Reorder enabled models (`scoped-models-selector.ts:343-366`).
        let reorder_up = theme::app_keys::matches(data, theme::app_keys::MODELS_REORDER_UP);
        let reorder_down = theme::app_keys::matches(data, theme::app_keys::MODELS_REORDER_DOWN);
        if reorder_up || reorder_down {
            if self.enabled_ids.is_none() {
                return;
            }
            let Some(item) = self.filtered_items.get(self.selected_index) else {
                return;
            };
            if is_enabled(&self.enabled_ids, &item.full_id) {
                let delta: isize = if reorder_up { -1 } else { 1 };
                let current_index = self
                    .enabled_ids
                    .as_ref()
                    .expect("checked is_none above")
                    .iter()
                    .position(|existing| existing == &item.full_id);
                let Some(current_index) = current_index else {
                    return;
                };
                let new_index = current_index as isize + delta;
                // Only move if within bounds
                let length =
                    self.enabled_ids.as_ref().expect("checked is_none above").len();
                if new_index >= 0 && new_index < length as isize {
                    self.enabled_ids = move_enabled(&self.enabled_ids, &item.full_id, delta);
                    self.is_dirty = true;
                    // Upstream adds the delta straight onto the selection
                    // (`scoped-models-selector.ts:359`); `refresh` clamps
                    // when the filtered row order drifts from the enabled
                    // order.
                    self.selected_index =
                        (self.selected_index as isize + delta).max(0) as usize;
                    self.refresh();
                    self.notify_change();
                }
            }
            return;
        }

        // Toggle on Enter (`scoped-models-selector.ts:368-378`).
        if pi_tui::keybindings::matches(data, pi_tui::keybindings::action::SELECT_CONFIRM) {
            if let Some(item) = self.filtered_items.get(self.selected_index) {
                let full_id = item.full_id.clone();
                self.enabled_ids = toggle(&self.enabled_ids, &self.all_ids, &full_id);
                self.is_dirty = true;
                self.refresh();
                self.notify_change();
            }
            return;
        }

        // Enable all (filtered if search active, otherwise all)
        // (`scoped-models-selector.ts:380-389`).
        if theme::app_keys::matches(data, theme::app_keys::MODELS_ENABLE_ALL) {
            let target_ids = self.filtered_target_ids();
            self.enabled_ids = enable_all(&self.enabled_ids, &self.all_ids, target_ids.as_deref());
            self.is_dirty = true;
            self.refresh();
            self.notify_change();
            return;
        }

        // Clear all (filtered if search active, otherwise all)
        // (`scoped-models-selector.ts:391-400`).
        if theme::app_keys::matches(data, theme::app_keys::MODELS_CLEAR_ALL) {
            let target_ids = self.filtered_target_ids();
            self.enabled_ids = clear_all(&self.enabled_ids, &self.all_ids, target_ids.as_deref());
            self.is_dirty = true;
            self.refresh();
            self.notify_change();
            return;
        }

        // Toggle provider of current item (`scoped-models-selector.ts:402-414`).
        if theme::app_keys::matches(data, theme::app_keys::MODELS_TOGGLE_PROVIDER) {
            let provider = self
                .filtered_items
                .get(self.selected_index)
                .and_then(|item| item.model.as_ref())
                .map(|model| model.provider.clone());
            if let Some(provider) = provider {
                let provider_ids: Vec<String> = self
                    .all_ids
                    .iter()
                    .filter(|id| {
                        self.models_by_id
                            .get(*id)
                            .is_some_and(|model| model.provider == provider)
                    })
                    .cloned()
                    .collect();
                let all_enabled = provider_ids
                    .iter()
                    .all(|id| is_enabled(&self.enabled_ids, id));
                self.enabled_ids = if all_enabled {
                    clear_all(&self.enabled_ids, &self.all_ids, Some(&provider_ids))
                } else {
                    enable_all(&self.enabled_ids, &self.all_ids, Some(&provider_ids))
                };
                self.is_dirty = true;
                self.refresh();
                self.notify_change();
            }
            return;
        }

        // Save/persist to settings (`scoped-models-selector.ts:416-421`).
        if theme::app_keys::matches(data, theme::app_keys::MODELS_SAVE) {
            self.events
                .lock()
                .expect("selector events")
                .push(ScopedModelsSelectorEvent::Persist(self.enabled_ids.clone()));
            self.is_dirty = false;
            return;
        }

        // Ctrl+C - clear search or cancel if empty
        // (`scoped-models-selector.ts:423-433`).
        if pi_tui::keys::matches_key(data, "ctrl+c") {
            if self.search_input.value().is_empty() {
                self.events
                    .lock()
                    .expect("selector events")
                    .push(ScopedModelsSelectorEvent::Cancel);
            } else {
                self.search_input.set_value("");
                self.refresh();
            }
            return;
        }

        // Escape - cancel (`scoped-models-selector.ts:435-438`).
        if pi_tui::keys::matches_key(data, "escape") {
            self.events
                .lock()
                .expect("selector events")
                .push(ScopedModelsSelectorEvent::Cancel);
            return;
        }

        // Pass everything else to search input
        // (`scoped-models-selector.ts:440-442`).
        Component::handle_input(&mut self.search_input, data);
        self.refresh();
    }
}

impl Component for ScopedModelsSelectorComponent {
    /// The constructor's mount sequence
    /// (`scoped-models-selector.ts:122-153`): border, header, hint, search
    /// input, list, refresh status, footer, border.
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let border = theme.fg_styler(ThemeColor::BorderMuted);
        let mut lines = vec![border(&"─".repeat(width.max(1)))];
        lines.push(String::new());
        lines.push(theme.fg(
            ThemeColor::Accent,
            &theme::bold("Model Configuration"),
        ));
        lines.push(theme.fg(
            ThemeColor::Muted,
            &format!(
                "Session-only. {} to save to settings.",
                theme::key_display_text(theme::app_keys::MODELS_SAVE)
            ),
        ));
        lines.push(String::new());
        lines.extend(self.search_input.render(width));
        lines.push(String::new());
        lines.extend(self.list_lines.iter().cloned());
        lines.push(String::new());
        if let Some((message, kind)) = &self.refresh_status {
            let color = match kind {
                RefreshStatusKind::Muted => ThemeColor::Muted,
                RefreshStatusKind::Success => ThemeColor::Success,
                RefreshStatusKind::Warning => ThemeColor::Warning,
            };
            lines.push(theme.fg(color, &format!("  {message}")));
        }
        lines.push(self.footer_text());
        lines.push(border(&"─".repeat(width.max(1))));
        lines
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        self.handle_selector_input(data);
        Some(ComponentMutation { consume: true })
    }

    fn set_focused(&mut self, focused: bool) {
        // Focusable: propagate to the search input for IME cursor
        // positioning (`scoped-models-selector.ts:141-149`).
        self.search_input.set_focused(focused);
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str, name: &str) -> pi_ai::types::Model {
        pi_ai::types::Model {
            provider: provider.to_string(),
            id: id.to_string(),
            name: name.to_string(),
            ..pi_ai::types::Model::default()
        }
    }

    fn config(models: &[pi_ai::types::Model], enabled: Option<Vec<String>>) -> ModelsConfig {
        ModelsConfig {
            all_models: models.to_vec(),
            enabled_model_ids: enabled,
            refresh_status: None,
        }
    }

    /// The upstream harness's callbacks
    /// (`scoped-models-selector.test.ts:44-57`): the model states mirror
    /// the Change events.
    fn harness(
        models: &[pi_ai::types::Model],
        enabled: Option<Vec<String>>,
    ) -> (
        ScopedModelsSelectorComponent,
        Arc<Mutex<Vec<ScopedModelsSelectorEvent>>>,
        Vec<bool>,
    ) {
        let provider = models[0].provider.clone();
        let initial: Vec<bool> = models
            .iter()
            .map(|model| {
                enabled
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&format!("{}/{}", provider, model.id)))
            })
            .collect();
        let events = Arc::new(Mutex::new(Vec::new()));
        let component =
            ScopedModelsSelectorComponent::new(config(models, enabled), Arc::clone(&events));
        (component, events, initial)
    }

    /// Apply the harness's onChange semantics to the state vector.
    fn states_from_events(
        initial: &[bool],
        events: &[ScopedModelsSelectorEvent],
        models: &[pi_ai::types::Model],
    ) -> Vec<bool> {
        let mut states = initial.to_vec();
        for event in events {
            let ScopedModelsSelectorEvent::Change(enabled) = event else {
                continue;
            };
            for (index, model) in models.iter().enumerate() {
                states[index] = enabled.as_ref().is_none_or(|list| {
                    list.contains(&format!("{}/{}", model.provider, model.id))
                });
            }
        }
        states
    }

    fn drained(events: &Arc<Mutex<Vec<ScopedModelsSelectorEvent>>>) -> Vec<ScopedModelsSelectorEvent> {
        events.lock().expect("events").clone()
    }

    fn plain(component: &ScopedModelsSelectorComponent) -> String {
        pi_tui::ansi::strip_terminal_sequences(&component.render(120).join("\n"))
    }

    fn marker_states(
        component: &ScopedModelsSelectorComponent,
        models: &[pi_ai::types::Model],
    ) -> Vec<bool> {
        // `getMarkerStates` (`scoped-models-selector.test.ts:26-34`): the
        // "✓ " marker's presence on each model's row — after slicing off
        // the two-char selection prefix (`line.slice(2)`).
        let lines: Vec<String> = plain(component).lines().map(str::to_string).collect();
        models
            .iter()
            .map(|model| {
                lines
                    .iter()
                    .find(|line| line.contains(&format!("{} [", model.id)))
                    .map(|line| line.chars().skip(2).collect::<String>().starts_with("✓ "))
                    .expect("rendered row")
            })
            .collect()
    }

    fn key_sequence(component: &mut ScopedModelsSelectorComponent, keys: &[&str]) {
        for key in keys {
            component.handle_selector_input(key);
        }
    }

    // The upstream `scoped models selector` tests
    // (`test/scoped-models-selector.test.ts`), mirrored behaviorally.

    // #5916
    #[test]
    fn shows_model_names_that_differ_from_the_id() {
        let models = vec![
            model("acme", "model-a", "Model A"),
            model("acme", "model-b", "Renamed B"),
        ];
        let (component, _events, _states) = harness(&models, None);
        let rows: Vec<String> = plain(&component)
            .lines()
            .map(|line| {
                line.chars().skip(4).collect::<String>().trim_end().to_string()
            })
            .collect();
        assert!(rows.contains(&"model-a [acme]".to_string()), "{rows:?}");
        assert!(
            rows.contains(&"model-b (Renamed B) [acme]".to_string()),
            "{rows:?}"
        );
    }

    #[test]
    fn marks_every_model_after_enabling_all() {
        let models = vec![
            model("acme", "model-a", "Model A"),
            model("acme", "model-b", "Model B"),
            model("acme", "model-c", "Model C"),
        ];
        let (mut component, events, initial) =
            harness(&models, Some(vec!["acme/model-a".to_string()]));
        key_sequence(&mut component, &["\x01"]); // ctrl+a: enable all

        let states = states_from_events(&initial, &drained(&events), &models);
        assert_eq!(states, vec![true, true, true]);
        assert_eq!(marker_states(&component, &models), vec![true, true, true]);
        assert!(plain(&component).contains("all enabled"));
    }

    #[test]
    fn disables_only_the_selected_model_after_enabling_all() {
        let models = vec![
            model("acme", "model-a", "Model A"),
            model("acme", "model-b", "Model B"),
            model("acme", "model-c", "Model C"),
        ];
        let (mut component, events, initial) =
            harness(&models, Some(vec!["acme/model-a".to_string()]));
        key_sequence(&mut component, &["\x01", "\r"]); // enable all, toggle model-a

        let states = states_from_events(&initial, &drained(&events), &models);
        assert_eq!(states, vec![false, true, true]);
        assert_eq!(marker_states(&component, &models), vec![false, true, true]);
    }

    #[test]
    fn enables_only_the_selected_model_after_clearing_all() {
        let models = vec![
            model("acme", "model-a", "Model A"),
            model("acme", "model-b", "Model B"),
            model("acme", "model-c", "Model C"),
        ];
        let (mut component, events, initial) = harness(&models, None);
        key_sequence(&mut component, &["\x18"]); // ctrl+x: clear all

        let states = states_from_events(&initial, &drained(&events), &models);
        assert_eq!(states, vec![false, false, false]);
        assert_eq!(marker_states(&component, &models), vec![false, false, false]);

        key_sequence(&mut component, &["\r"]); // toggle model-a
        let states = states_from_events(&initial, &drained(&events), &models);
        assert_eq!(states, vec![true, false, false]);
        assert_eq!(marker_states(&component, &models), vec![true, false, false]);
    }

    #[test]
    fn restores_the_all_enabled_state_after_re_enabling_the_last_disabled_model() {
        let models = vec![
            model("acme", "model-a", "Model A"),
            model("acme", "model-b", "Model B"),
            model("acme", "model-c", "Model C"),
        ];
        let (mut component, events, initial) =
            harness(&models, Some(vec!["acme/model-a".to_string()]));
        // enable all -> null; disable model-a (enabled models re-sort first:
        // [b, c, a]); move the selection back to model-a; re-enable.
        key_sequence(&mut component, &["\x01", "\r", "\x1b[B", "\x1b[B", "\r"]);

        let states = states_from_events(&initial, &drained(&events), &models);
        assert_eq!(states, vec![true, true, true]);
        assert_eq!(marker_states(&component, &models), vec![true, true, true]);
        assert!(plain(&component).contains("all enabled"));
    }

    #[test]
    fn save_publishes_persist_and_clears_the_dirty_marker() {
        let models = vec![model("acme", "model-a", "Model A")];
        let (mut component, events, _states) =
            harness(&models, Some(vec!["acme/model-a".to_string()]));
        key_sequence(&mut component, &["\r"]); // toggle → dirty
        assert!(plain(&component).contains("(unsaved)"));
        key_sequence(&mut component, &["\x13"]); // ctrl+s: save
        let drained = drained(&events);
        assert!(matches!(
            drained.last(),
            Some(ScopedModelsSelectorEvent::Persist(Some(_)))
        ));
        assert!(!plain(&component).contains("(unsaved)"));
    }

    #[test]
    fn escape_and_ctrl_c_cancel_when_the_search_is_empty() {
        let models = vec![model("acme", "model-a", "Model A")];
        let (mut component, events, _states) = harness(&models, None);
        component.handle_input("\x1b");
        assert_eq!(
            drained(&events),
            vec![ScopedModelsSelectorEvent::Cancel]
        );
        component.handle_input("\x03");
        assert_eq!(drained(&events).len(), 2);
    }

    #[test]
    fn ctrl_c_clears_a_filled_search_instead_of_cancelling() {
        let models = vec![model("acme", "model-a", "Model A")];
        let (mut component, events, _states) = harness(&models, None);
        component.handle_input("mod");
        assert_ne!(component.search_input.value(), "");
        component.handle_input("\x03");
        assert_eq!(component.search_input.value(), "");
        assert_eq!(drained(&events).len(), 0);
    }

    #[test]
    fn unavailable_patterns_render_the_unavailable_badge_and_count() {
        let models = vec![model("acme", "model-a", "Model A")];
        let (component, _events, _states) = harness(
            &models,
            Some(vec![
                "acme/model-a".to_string(),
                "ghost/missing".to_string(),
            ]),
        );
        let text = plain(&component);
        assert!(text.contains("[unavailable]"), "{text}");
        // `${enabledCount}/${this.allIds.length}` — allIds holds only the
        // available models; the ghost pattern counts separately
        // (`scoped-models-selector.ts:203-209`).
        assert!(text.contains("1/1 enabled · 1 unavailable"), "{text}");
    }

    #[test]
    fn search_filters_the_list_and_scopes_enable_all_to_the_matches() {
        let models = vec![
            model("acme", "model-a", "Model A"),
            model("acme", "model-b", "Model B"),
            // A provider whose haystack cannot fuzzy-match the query.
            model("gamma", "model-c", "Model C"),
        ];
        let (mut component, events, _initial) =
            harness(&models, Some(vec!["acme/model-a".to_string()]));
        for c in "b".chars() {
            component.handle_input(&c.to_string());
        }
        // enable all → the filtered row joins the explicit list
        // (`scoped-models-selector.ts:383`: targets are the filtered rows;
        // from `null` upstream returns `null` unchanged, `:39`).
        key_sequence(&mut component, &["\x01"]);
        let drained = drained(&events);
        let Some(ScopedModelsSelectorEvent::Change(Some(ids))) = drained.last() else {
            panic!("expected a change, got {drained:?}");
        };
        assert_eq!(ids, &["acme/model-a".to_string(), "acme/model-b".to_string()]);
        let text = plain(&component);
        assert!(text.contains("model-b"), "{text}");
        assert!(!text.contains("Model Name: Model A"), "{text}");
    }

    #[test]
    fn reorder_moves_enabled_entries_within_the_explicit_list() {
        let models = vec![model("acme", "one", "One"), model("acme", "two", "Two")];
        let (mut component, events, _states) = harness(
            &models,
            Some(vec!["acme/one".to_string(), "acme/two".to_string()]),
        );
        // Selection starts at "one"; alt+down (`\x1bn`) moves it below "two".
        key_sequence(&mut component, &["\x1bn"]);
        let drained = drained(&events);
        let Some(ScopedModelsSelectorEvent::Change(Some(ids))) = drained.last() else {
            panic!("expected a change, got {drained:?}");
        };
        assert_eq!(ids, &["acme/two".to_string(), "acme/one".to_string()]);
    }

    #[test]
    fn provider_toggle_disables_a_fully_enabled_provider() {
        let models = vec![
            model("acme", "one", "One"),
            model("acme", "two", "Two"),
            model("beta", "three", "Three"),
        ];
        let (mut component, events, _states) = harness(&models, None);
        key_sequence(&mut component, &["\x10"]); // ctrl+p: toggle provider (all enabled → clear acme)
        let drained = drained(&events);
        let Some(ScopedModelsSelectorEvent::Change(Some(ids))) = drained.last() else {
            panic!("expected a change, got {drained:?}");
        };
        assert_eq!(ids, &["beta/three".to_string()]);
    }

    #[test]
    fn refresh_status_renders_with_its_kind() {
        let models = vec![model("acme", "one", "One")];
        let (mut component, _events, _states) = harness(&models, None);
        component.set_refresh_status("Model catalogs refreshed.", RefreshStatusKind::Success);
        let text = plain(&component);
        assert!(text.contains("Model catalogs refreshed."), "{text}");
    }

    #[test]
    fn update_models_keeps_the_selected_row_and_swaps_the_set() {
        let models = vec![model("acme", "one", "One"), model("acme", "two", "Two")];
        let (mut component, _events, _states) = harness(&models, None);
        key_sequence(&mut component, &["\x1b[B"]); // select "two"
        let refreshed = vec![model("acme", "two", "Two"), model("acme", "new", "New")];
        component.update_models(&refreshed, None);
        let text = plain(&component);
        assert!(text.contains("Model Name: Two"), "{text}");
        assert!(!text.contains("Model Name: One"), "{text}");
        assert!(text.contains("new"), "{text}");
    }

    // The pure algebra the component leans on
    // (`scoped-models-selector.ts:16-78`).

    #[test]
    fn toggle_normalizes_back_to_all_enabled() {
        let all = vec!["a".to_string(), "b".to_string()];
        // Explicit [b], re-enable a → covers everything → null.
        assert_eq!(toggle(&Some(vec!["b".to_string()]), &all, "a"), None);
        // Null, disable a → [b].
        assert_eq!(toggle(&None, &all, "a"), Some(vec!["b".to_string()]));
    }

    #[test]
    fn enable_all_is_a_noop_from_null_and_targets_the_filtered_rows() {
        let all = vec!["a".to_string(), "b".to_string()];
        assert_eq!(enable_all(&None, &all, None), None);
        assert_eq!(
            enable_all(&Some(vec!["a".to_string()]), &all, Some(&["b".to_string()])),
            None
        );
    }

    #[test]
    fn clear_all_from_null_with_targets_excludes_them() {
        let all = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(
            clear_all(&None, &all, Some(&["b".to_string()])),
            Some(vec!["a".to_string(), "c".to_string()])
        );
        assert_eq!(clear_all(&None, &all, None), Some(Vec::new()));
        assert_eq!(
            clear_all(&Some(vec!["a".to_string(), "b".to_string()]), &all, None),
            Some(Vec::new())
        );
    }

    #[test]
    fn move_only_swaps_within_bounds() {
        let ids = Some(vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(
            move_enabled(&ids, "a", -1),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        assert_eq!(
            move_enabled(&ids, "b", -1),
            Some(vec!["b".to_string(), "a".to_string(), "c".to_string()])
        );
        assert_eq!(move_enabled(&None, "a", 1), None);
    }

    #[test]
    fn sorted_ids_put_the_explicit_order_first() {
        let all = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let enabled = Some(vec!["c".to_string()]);
        assert_eq!(
            get_sorted_ids(&enabled, &all),
            vec![
                "c".to_string(),
                "a".to_string(),
                "b".to_string()
            ]
        );
        assert_eq!(get_sorted_ids(&None, &all), all);
    }
}
