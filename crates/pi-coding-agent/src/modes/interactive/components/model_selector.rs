//! `ModelSelectorComponent` — the `/model` search dialog.
//!
//! Ground truth: `modes/interactive/components/model-selector.ts`. Deviations
//! for this phase: the model catalog is embedded and static, so there is no
//! background catalog refresh or refresh status; scoped models
//! (`/scoped-models`) and the save-as-default binding land with the settings
//! work, so the scope toggle, default badge, and `app.models.save` binding
//! are absent and the hint line shows the unscoped notice.

use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::input::{Input, InputOptions};
use pi_tui::keybindings;
use pi_tui::keybindings::action;
use pi_tui::theme::ThemeColor;

use crate::modes::interactive::theme;

/// The window of models shown at once (`MAX_VISIBLE`, `model-selector.ts:38`).
const MAX_VISIBLE: usize = 10;

/// Events the selector publishes to the orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelSelectorEvent {
    /// `onSelect` — the highlighted model was confirmed.
    Selected(pi_ai::types::Model),
    /// `onCancel` — escape/ctrl+c.
    Cancelled,
}

/// `getSelectPageIndex` (`packages/tui/src/keybindings.ts:327-341`): the page
/// step for `tui.select.pageUp`/`pageDown`, or none for other keys.
fn get_select_page_index(
    data: &str,
    selected_index: usize,
    item_count: usize,
    page_size: usize,
) -> Option<usize> {
    let direction: isize = if keybindings::matches(data, action::SELECT_PAGE_UP) {
        -1
    } else if keybindings::matches(data, action::SELECT_PAGE_DOWN) {
        1
    } else {
        return None;
    };
    let next = selected_index as isize + direction * page_size as isize;
    Some(next.clamp(0, item_count as isize - 1) as usize)
}

/// `getDistinctModelName` (`model-search.ts:25-34`): the model name when it
/// has words the id does not.
fn get_distinct_model_name(id: &str, name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let id_words: Vec<String> = id
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let id_tokens: std::collections::HashSet<&String> = id_words.iter().collect();
    let compact_id: String = id_words.concat();
    let is_covered = |word: &String| {
        id_tokens.contains(word) || (word.len() > 1 && compact_id.contains(word.as_str()))
    };
    let name_words: Vec<String> = name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    if name_words.iter().all(is_covered) {
        None
    } else {
        Some(name.to_string())
    }
}

/// `getModelSelectorSearchText` (`model-search.ts:40-44`): the provider comes
/// first so exact provider-prefixed queries rank before proxy-provider ids.
fn get_model_selector_search_text(id: &str, provider: &str, name: &str) -> String {
    if name.is_empty() {
        format!("{provider} {provider}/{id} {provider} {id}")
    } else {
        format!("{provider} {provider}/{id} {provider} {id} {name}")
    }
}

/// The selector dialog (`ModelSelectorComponent`, `model-selector.ts:43-439`).
pub struct ModelSelectorComponent {
    search_input: Input,
    all_models: Vec<pi_ai::types::Model>,
    filtered: Vec<pi_ai::types::Model>,
    selected_index: usize,
    current_provider: String,
    current_id: String,
    /// The rendered list rows, rebuilt by `update_list`.
    list_lines: Vec<String>,
    events: Arc<Mutex<Vec<ModelSelectorEvent>>>,
}

impl ModelSelectorComponent {
    /// The constructor (`model-selector.ts:80-163`) minus the refresh and
    /// scoped-model machinery.
    pub fn new(
        current: &pi_ai::types::Model,
        available: Vec<pi_ai::types::Model>,
        initial_search: Option<&str>,
        events: Arc<Mutex<Vec<ModelSelectorEvent>>>,
    ) -> Self {
        let mut selector = ModelSelectorComponent {
            search_input: Input::new(InputOptions::default()),
            all_models: Vec::new(),
            filtered: Vec::new(),
            selected_index: 0,
            current_provider: current.provider.clone(),
            current_id: current.id.clone(),
            list_lines: Vec::new(),
            events,
        };
        selector.all_models =
            sort_models(available, &selector.current_provider, &selector.current_id);
        // The current model starts selected (`model-selector.ts:183-185`).
        selector.selected_index = selector
            .all_models
            .iter()
            .position(|m| m.provider == selector.current_provider && m.id == selector.current_id)
            .unwrap_or(0);
        if let Some(search) = initial_search {
            selector.search_input.set_value(search);
        }
        let query = selector.search_input.value().to_string();
        selector.filter_models(&query, None);
        selector
    }

    /// Take the queued events (the orchestrator drains after each pump).
    pub fn take_events(&mut self) -> Vec<ModelSelectorEvent> {
        std::mem::take(self.events.lock().expect("selector events").as_mut())
    }

    /// `filterModels` (`model-selector.ts:285-313`) without the refresh
    /// preference (the catalog never refreshes here).
    fn filter_models(&mut self, query: &str, preferred: Option<&pi_ai::types::Model>) {
        self.filtered = if query.is_empty() {
            self.all_models.clone()
        } else {
            pi_tui::fuzzy::fuzzy_filter(&self.all_models, query, |model| {
                get_model_selector_search_text(&model.id, &model.provider, &model.name)
            })
        };
        // Keep the preferred model selected if it is still listed; otherwise
        // a query moves the selector to the top row, and clearing the query
        // keeps the position clamped (`model-selector.ts:304-311`).
        self.selected_index = match preferred.and_then(|model| {
            self.filtered
                .iter()
                .position(|item| item.provider == model.provider && item.id == model.id)
        }) {
            Some(index) => index,
            None if !query.is_empty() => 0,
            None => self
                .selected_index
                .min(self.filtered.len().saturating_sub(1)),
        };
        self.update_list();
    }

    /// `updateList` (`model-selector.ts:315-371`).
    fn update_list(&mut self) {
        let theme = theme::theme();
        self.list_lines.clear();

        let length = self.filtered.len();
        let start = self
            .selected_index
            .saturating_sub(MAX_VISIBLE / 2)
            .min(length.saturating_sub(MAX_VISIBLE));
        let end = (start + MAX_VISIBLE).min(length);

        for index in start..end {
            let item = &self.filtered[index];
            let is_selected = index == self.selected_index;
            let is_current = item.provider == self.current_provider && item.id == self.current_id;

            let cursor = if is_selected {
                theme.fg(ThemeColor::Accent, "→ ")
            } else {
                "  ".to_string()
            };
            let current_marker = if is_current {
                theme.fg(ThemeColor::Accent, "✓ ")
            } else {
                "  ".to_string()
            };
            let model_text = if is_selected {
                theme.fg(ThemeColor::Accent, &item.id)
            } else {
                item.id.clone()
            };
            let name_text = match get_distinct_model_name(&item.id, &item.name) {
                Some(name) => theme.fg(ThemeColor::Muted, &format!(" ({name})")),
                None => String::new(),
            };
            let provider_badge = theme.fg(ThemeColor::Muted, &format!("[{}]", item.provider));
            self.list_lines.push(format!(
                "{cursor}{current_marker}{model_text}{name_text} {provider_badge}"
            ));
        }

        if start > 0 || end < length {
            let scroll_info = theme.fg(
                ThemeColor::Muted,
                &format!("  ({}/{length})", self.selected_index + 1),
            );
            self.list_lines.push(scroll_info);
        }

        if self.filtered.is_empty() {
            self.list_lines
                .push(theme.fg(ThemeColor::Muted, "  No matching models"));
        } else {
            let selected = &self.filtered[self.selected_index];
            self.list_lines.push(String::new());
            self.list_lines.push(theme.fg(
                ThemeColor::Muted,
                &format!("  Model Name: {}", selected.name),
            ));
        }
    }

    /// `handleSelect` / `onCancelCallback`: publish and let the orchestrator
    /// close the dialog.
    fn publish(&mut self, event: ModelSelectorEvent) {
        self.events.lock().expect("selector events").push(event);
    }
}

/// `sortModels` (`model-selector.ts:237-252`): current model first, then by
/// provider. (The default-model slot lands with settings persistence.)
fn sort_models(
    mut models: Vec<pi_ai::types::Model>,
    current_provider: &str,
    current_id: &str,
) -> Vec<pi_ai::types::Model> {
    models.sort_by(|a, b| {
        let a_is_current = a.provider == current_provider && a.id == current_id;
        let b_is_current = b.provider == current_provider && b.id == current_id;
        b_is_current
            .cmp(&a_is_current)
            .then_with(|| a.provider.cmp(&b.provider))
    });
    models
}

impl Component for ModelSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let border = theme.fg_styler(ThemeColor::BorderMuted);
        let mut lines = vec![border(&"─".repeat(width.max(1)))];
        lines.push(String::new());
        lines.push(theme.fg(
            ThemeColor::Warning,
            "Only showing models from configured providers. Use /login to add providers.",
        ));
        lines.push(String::new());
        lines.extend(self.search_input.render(width));
        lines.push(String::new());
        lines.extend(self.list_lines.iter().cloned());
        lines.push(String::new());
        lines.push(border(&"─".repeat(width.max(1))));
        lines
    }

    /// `handleInput` (`model-selector.ts:373-429`).
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        // Tab toggles all/scoped; with no scoped models it is consumed unused
        // (`model-selector.ts:376-385`).
        if keybindings::matches(data, action::INPUT_TAB) {
            return Some(ComponentMutation { consume: true });
        }
        if keybindings::matches(data, action::SELECT_UP) {
            if self.filtered.is_empty() {
                return Some(ComponentMutation { consume: true });
            }
            self.selected_index = if self.selected_index == 0 {
                self.filtered.len() - 1
            } else {
                self.selected_index - 1
            };
            self.update_list();
            return Some(ComponentMutation { consume: true });
        }
        if keybindings::matches(data, action::SELECT_DOWN) {
            if self.filtered.is_empty() {
                return Some(ComponentMutation { consume: true });
            }
            self.selected_index = if self.selected_index == self.filtered.len() - 1 {
                0
            } else {
                self.selected_index + 1
            };
            self.update_list();
            return Some(ComponentMutation { consume: true });
        }
        if let Some(page_index) =
            get_select_page_index(data, self.selected_index, self.filtered.len(), MAX_VISIBLE)
        {
            if self.filtered.is_empty() {
                return Some(ComponentMutation { consume: true });
            }
            self.selected_index = page_index;
            self.update_list();
            return Some(ComponentMutation { consume: true });
        }
        if keybindings::matches(data, action::SELECT_CONFIRM) {
            if let Some(model) = self.filtered.get(self.selected_index).cloned() {
                self.publish(ModelSelectorEvent::Selected(model));
            }
            return Some(ComponentMutation { consume: true });
        }
        if keybindings::matches(data, action::SELECT_CANCEL) {
            self.publish(ModelSelectorEvent::Cancelled);
            return Some(ComponentMutation { consume: true });
        }
        // Everything else reaches the search input.
        let mutation = Component::handle_input(&mut self.search_input, data);
        let query = self.search_input.value().to_string();
        self.filter_models(&query, None);
        mutation
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

    fn selector(
        models: Vec<pi_ai::types::Model>,
        current: &pi_ai::types::Model,
    ) -> (ModelSelectorComponent, Arc<Mutex<Vec<ModelSelectorEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let component = ModelSelectorComponent::new(current, models, None, Arc::clone(&events));
        (component, events)
    }

    fn plain(component: &ModelSelectorComponent) -> String {
        pi_tui::ansi::strip_terminal_sequences(&component.render(80).join("\n"))
    }

    #[test]
    fn sort_puts_current_first_then_providers() {
        let models = vec![
            model("zebra", "z1", ""),
            model("anthropic", "a1", ""),
            model("anthropic", "a2", ""),
        ];
        let current = model("anthropic", "a2", "");
        let sorted = sort_models(models, &current.provider, &current.id);
        assert_eq!(sorted[0].id, "a2");
        assert_eq!(sorted[1].provider, "anthropic");
        assert_eq!(sorted[2].provider, "zebra");
    }

    #[test]
    fn renders_rows_with_cursor_and_current_marker() {
        let models = vec![
            model("anthropic", "claude-a", "Claude A"),
            model("openai", "gpt-5", "GPT-5"),
        ];
        let current = model("openai", "gpt-5", "GPT-5");
        let (component, _events) = selector(models, &current);
        let text = plain(&component);
        assert!(text.contains("✓ gpt-5"), "{text}");
        assert!(text.contains("[openai]"), "{text}");
        assert!(text.contains("claude-a"), "{text}");
        assert!(text.contains("Model Name: GPT-5"), "{text}");
        assert!(
            text.contains("Only showing models from configured providers"),
            "{text}"
        );
    }

    #[test]
    fn arrows_wrap_and_enter_publishes() {
        let models = vec![model("a", "one", ""), model("b", "two", "")];
        let current = model("a", "one", "");
        let (mut component, events) = selector(models, &current);
        component.handle_input("\x1b[B"); // down → index 1
        component.handle_input("\r"); // enter
        let events = events.lock().expect("events").clone();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ModelSelectorEvent::Selected(model) => assert_eq!(model.id, "two"),
            ModelSelectorEvent::Cancelled => panic!("expected selection"),
        }
    }

    #[test]
    fn escape_publishes_cancel() {
        let models = vec![model("a", "one", "")];
        let current = model("a", "one", "");
        let (mut component, events) = selector(models, &current);
        component.handle_input("\x1b");
        assert_eq!(
            events.lock().expect("events").clone(),
            vec![ModelSelectorEvent::Cancelled]
        );
    }

    #[test]
    fn search_filters_and_no_match_shows_placeholder() {
        let models = vec![model("a", "one", ""), model("b", "two", "")];
        let current = model("a", "one", "");
        let (mut component, _events) = selector(models, &current);
        for c in "two".chars() {
            component.handle_input(c.to_string().as_str());
        }
        let text = plain(&component);
        assert!(text.contains("two"), "{text}");
        assert!(!text.contains("Model Name: one"), "{text}");

        let (mut component, _events) =
            selector(vec![model("a", "one", "")], &model("a", "one", ""));
        for c in "zzz".chars() {
            component.handle_input(c.to_string().as_str());
        }
        let text = plain(&component);
        assert!(text.contains("No matching models"), "{text}");
    }

    #[test]
    fn distinct_model_name_only_when_it_adds_words() {
        assert_eq!(get_distinct_model_name("minimax-m3", "MiniMax M3"), None);
        assert_eq!(
            get_distinct_model_name("claude-sonnet-4-5", "Claude Sonnet 4.5"),
            None
        );
        assert_eq!(
            get_distinct_model_name("gpt-5", "GPT-5 Thinking"),
            Some("GPT-5 Thinking".to_string())
        );
        // Single-character words can only match a whole id token, so a name
        // word like "m3" stays uncovered and the name is shown
        // ("model-search.ts:31-32").
        assert_eq!(
            get_distinct_model_name("mistral-small-2603", "M3"),
            Some("M3".to_string())
        );
        assert_eq!(get_distinct_model_name("m3", "M3"), None);
    }
}
