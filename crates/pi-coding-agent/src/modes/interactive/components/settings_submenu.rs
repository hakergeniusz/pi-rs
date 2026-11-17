//! `SelectSubmenu` and `SteppedSubmenu` — the settings dialogs' submenus.
//!
//! Ground truth: `modes/interactive/components/settings-submenu.js`. Events
//! reach owners through the queue they pass in — the port's `SelectList` is
//! action-based, so the submenu translates actions after each key and the
//! owner drains, where pi wires `onSelect`/`onCancel`/`onSelectionChange`
//! callbacks into the list.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::input::{Input, InputAction, InputOptions};
use pi_tui::components::select_list::{SelectItem, SelectList, SelectListAction, SelectListLayout};
use pi_tui::fuzzy::fuzzy_filter;
use pi_tui::keybindings;
use pi_tui::keybindings::action;
use pi_tui::theme::ThemeColor;

use crate::modes::interactive::theme;

/// The submenu's published actions (`selectList.onSelect`/`onCancel`/
/// `onSelectionChange`, `settings-submenu.ts:61-65`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectSubmenuEvent {
    /// Enter on an option.
    Select(String),
    /// Escape/ctrl+c.
    Cancel,
    /// The highlighted option moved.
    SelectionChanged(String),
}

/// The default column bounds. `settings-submenu.ts:3-6`.
pub fn submenu_select_list_layout() -> SelectListLayout {
    SelectListLayout {
        min_primary_column_width: Some(12),
        max_primary_column_width: Some(32),
    }
}

/// A titled select list, optionally searchable. `settings-submenu.ts:11-98`.
pub struct SelectSubmenu {
    title: String,
    description: Option<String>,
    all_options: Vec<SelectItem>,
    select_list: SelectList,
    list_layout: SelectListLayout,
    search_input: Option<Input>,
    /// The done callback from the settings list, when the submenu was opened
    /// by one: `close` updates the item and closes the submenu
    /// (`settings-list.ts:212-221`).
    done: Option<pi_tui::components::settings_list::SubmenuDone>,
    /// Whether select/cancel close through `done` themselves. The theme
    /// submenu's single-mode select manages its done manually (picking
    /// "automatic" must not close); the theme picker selects do.
    auto_close: bool,
    events: Arc<Mutex<Vec<SelectSubmenuEvent>>>,
}

impl SelectSubmenu {
    /// `settings-submenu.ts:20-55`.
    pub fn new(
        title: &str,
        description: Option<&str>,
        options: Vec<SelectItem>,
        preselect: Option<&str>,
        events: Arc<Mutex<Vec<SelectSubmenuEvent>>>,
    ) -> Self {
        let select_list =
            Self::build_select_list(options.clone(), preselect, submenu_select_list_layout());
        Self {
            title: title.to_string(),
            description: description.map(str::to_string),
            all_options: options,
            select_list,
            list_layout: submenu_select_list_layout(),
            search_input: None,
            done: None,
            auto_close: false,
            events,
        }
    }

    /// `submenuOptions.searchable` (`settings-submenu.ts:34-42`).
    #[must_use]
    pub fn with_search(mut self) -> Self {
        self.search_input = Some(Input::new(InputOptions::default()));
        self
    }

    /// `submenuOptions.layout` (`settings-submenu.ts:23`).
    #[must_use]
    pub fn with_layout(mut self, layout: SelectListLayout) -> Self {
        self.list_layout = layout;
        self.select_list =
            Self::build_select_list(self.all_options.clone(), self.preselect_value(), layout);
        self
    }

    /// Hold the settings-list done callback and close through it on
    /// select/cancel (`settings-list.ts:212-221` semantics).
    #[must_use]
    pub fn with_done(mut self, done: pi_tui::components::settings_list::SubmenuDone) -> Self {
        self.done = Some(done);
        self.auto_close = true;
        self
    }

    /// Hold the done callback but leave its timing to the owner — the theme
    /// submenu's single-mode select.
    #[must_use]
    pub fn with_done_manual(
        mut self,
        done: pi_tui::components::settings_list::SubmenuDone,
    ) -> Self {
        self.done = Some(done);
        self
    }

    /// The value the current list was preselected with, if any.
    fn preselect_value(&self) -> Option<&str> {
        self.select_list
            .selected_item()
            .map(|item| item.value.as_str())
    }

    /// `buildSelectList` (`settings-submenu.ts:56-68`): a fresh list with the
    /// preselect moved to the matching option.
    fn build_select_list(
        options: Vec<SelectItem>,
        preselect: Option<&str>,
        layout: SelectListLayout,
    ) -> SelectList {
        let preselect_index = preselect
            .and_then(|preselect| options.iter().position(|option| option.value == preselect));
        let mut list = SelectList::new(options, 10, theme::select_list_theme(), layout);
        if let Some(index) = preselect_index {
            list.set_selected_index(index);
        }
        list
    }

    /// `applyFilter` (`settings-submenu.ts:69-76`): rebuild the list from a
    /// fuzzy match over `label description`.
    fn apply_filter(&mut self, query: &str) {
        let filtered = if query.is_empty() {
            self.all_options.clone()
        } else {
            fuzzy_filter(&self.all_options, query, |item| {
                format!(
                    "{} {}",
                    item.label,
                    item.description.clone().unwrap_or_default()
                )
            })
        };
        self.select_list = Self::build_select_list(filtered, None, self.list_layout);
    }

    fn push(&self, event: SelectSubmenuEvent) {
        self.events.lock().expect("submenu events").push(event);
    }

    /// Take the events published since the last drain.
    pub fn drain(&self) -> Vec<SelectSubmenuEvent> {
        std::mem::take(&mut *self.events.lock().expect("submenu events"))
    }

    /// `handleInput` (`settings-submenu.ts:77-97`).
    pub fn handle_key(&mut self, data: &str) {
        if let Some(input) = self.search_input.as_mut() {
            let is_nav = keybindings::matches(data, action::SELECT_UP)
                || keybindings::matches(data, action::SELECT_DOWN)
                || keybindings::matches(data, action::SELECT_PAGE_UP)
                || keybindings::matches(data, action::SELECT_PAGE_DOWN)
                || keybindings::matches(data, action::SELECT_CONFIRM)
                || keybindings::matches(data, action::SELECT_CANCEL);
            if is_nav {
                self.handle_list_key(data);
            } else {
                let submitted = matches!(input.handle_key(data), InputAction::Submitted(_));
                let query = input.value().to_string();
                if submitted {
                    // `searchInput.onSubmit` (`settings-submenu.ts:38-40`).
                    self.handle_list_key("\r");
                } else {
                    self.apply_filter(&query);
                }
            }
        } else {
            self.handle_list_key(data);
        }
    }

    /// Forward a key to the list and translate its action into events.
    fn handle_list_key(&mut self, data: &str) {
        let list_action = self.select_list.handle_key(data);
        match list_action {
            SelectListAction::Confirmed => {
                if let Some(item) = self.select_list.selected_item() {
                    let value = item.value.clone();
                    if self.auto_close
                        && let Some(done) = &self.done
                    {
                        done.close(Some(&value), None);
                    }
                    self.push(SelectSubmenuEvent::Select(value));
                }
            }
            SelectListAction::Cancelled => {
                if self.auto_close
                    && let Some(done) = &self.done
                {
                    done.close(None, None);
                }
                self.push(SelectSubmenuEvent::Cancel);
            }
            SelectListAction::SelectionChanged => {
                if let Some(item) = self.select_list.selected_item() {
                    self.push(SelectSubmenuEvent::SelectionChanged(item.value.clone()));
                }
            }
            SelectListAction::None => {}
        }
    }
}

impl Component for SelectSubmenu {
    /// `settings-submenu.ts:27-54` (the container's children).
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let mut lines = Vec::new();
        lines.push(theme.fg(ThemeColor::Accent, &theme::bold(&self.title)));
        if let Some(description) = &self.description {
            lines.push(String::new());
            lines.push(theme.fg(ThemeColor::Muted, description));
        }
        if let Some(input) = &self.search_input {
            lines.push(String::new());
            lines.extend(input.render(width));
        }
        lines.push(String::new());
        lines.extend(self.select_list.render(width));
        lines.push(String::new());
        let hint = if self.search_input.is_some() {
            "  Type to filter · Enter to select · Esc to go back"
        } else {
            "  Enter to select · Esc to go back"
        };
        lines.push(theme.fg(ThemeColor::Dim, hint));
        lines
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        self.handle_key(data);
        Some(ComponentMutation { consume: true })
    }

    fn handle_mouse(
        &mut self,
        event: &pi_tui::mouse::MouseEvent,
    ) -> Option<pi_tui::mouse::MouseEventResult> {
        self.select_list.handle_mouse(event)
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// The shared selections a stepped submenu builds up. `settings-submenu.ts:112`.
pub type SubmenuContext = BTreeMap<String, String>;

/// A step's texts/options, evaluated against the selections so far
/// (`settings-submenu.ts:397-467`).
pub type StepText = Box<dyn Fn(&SubmenuContext) -> String + Send + Sync>;
pub type StepOptions = Box<dyn Fn(&SubmenuContext) -> Vec<SelectItem> + Send + Sync>;
pub type StepPreselect = Box<dyn Fn(&SubmenuContext) -> Option<String> + Send + Sync>;

/// One step. `settings-submenu.ts` (the `steps` entries).
pub struct SubmenuStep {
    pub key: &'static str,
    pub title: StepText,
    pub description: StepText,
    pub options: StepOptions,
    pub preselect: Option<StepPreselect>,
    pub searchable: bool,
    pub layout: Option<SelectListLayout>,
}

/// The stepped submenu's published actions (`onComplete`/`onCancel`,
/// `settings-submenu.ts:469-483`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteppedSubmenuEvent {
    /// All steps completed; the selections so far.
    Completed(SubmenuContext),
    /// Escape at the first step.
    Cancel,
}

/// An N-step submenu: each selection advances, escape goes back one step;
/// with `loop`, completing the final step delivers the context and returns
/// to step 0. `settings-submenu.ts:106-166`.
pub struct SteppedSubmenu {
    steps: Vec<SubmenuStep>,
    loop_selections: bool,
    context: SubmenuContext,
    step_index: usize,
    active: SelectSubmenu,
    events: Arc<Mutex<Vec<SteppedSubmenuEvent>>>,
}

impl SteppedSubmenu {
    /// `settings-submenu.ts:113-121`.
    pub fn new(
        steps: Vec<SubmenuStep>,
        loop_selections: bool,
        events: Arc<Mutex<Vec<SteppedSubmenuEvent>>>,
    ) -> Self {
        let active = Self::build_step(&steps, 0, &SubmenuContext::new());
        Self {
            steps,
            loop_selections,
            context: SubmenuContext::new(),
            step_index: 0,
            active,
            events,
        }
    }

    /// `buildStep` (`settings-submenu.ts:122-156`).
    fn build_step(
        steps: &[SubmenuStep],
        step_index: usize,
        context: &SubmenuContext,
    ) -> SelectSubmenu {
        let step = &steps[step_index];
        let total = steps.len();
        let step_label = if total > 1 {
            format!("Step {}/{} · ", step_index + 1, total)
        } else {
            String::new()
        };
        let title = format!("{}{}", step_label, (step.title)(context));
        let description = (step.description)(context);
        let items = (step.options)(context);
        let preselect = step
            .preselect
            .as_ref()
            .and_then(|preselect| preselect(context));
        let events: Arc<Mutex<Vec<SelectSubmenuEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let mut submenu = SelectSubmenu::new(
            &title,
            Some(&description),
            items,
            preselect.as_deref(),
            events,
        );
        if step.searchable {
            submenu = submenu.with_search();
        }
        if let Some(layout) = step.layout {
            submenu = submenu.with_layout(layout);
        }
        submenu
    }

    fn push(&self, event: SteppedSubmenuEvent) {
        self.events.lock().expect("stepped events").push(event);
    }

    /// Take the events published since the last drain.
    pub fn drain(&self) -> Vec<SteppedSubmenuEvent> {
        std::mem::take(&mut *self.events.lock().expect("stepped events"))
    }

    /// `handleInput` (`settings-submenu.ts:160-162`) plus the callback
    /// reactions (`settings-submenu.ts:130-155`).
    pub fn handle_key(&mut self, data: &str) {
        self.active.handle_key(data);
        for event in self.active.drain() {
            match event {
                SelectSubmenuEvent::Select(value) => {
                    let step_index = self.step_index;
                    let key = self.steps[step_index].key;
                    self.context.insert(key.to_string(), value);
                    if step_index < self.steps.len() - 1 {
                        // Advance to next step.
                        self.step_index = step_index + 1;
                        self.active = Self::build_step(&self.steps, self.step_index, &self.context);
                    } else {
                        // Final step — deliver result.
                        self.push(SteppedSubmenuEvent::Completed(self.context.clone()));
                        if self.loop_selections {
                            self.context = SubmenuContext::new();
                            self.step_index = 0;
                            self.active = Self::build_step(&self.steps, 0, &self.context);
                        } else {
                            self.push(SteppedSubmenuEvent::Cancel);
                        }
                    }
                }
                SelectSubmenuEvent::Cancel => {
                    if self.step_index > 0 {
                        self.context.remove(self.steps[self.step_index].key);
                        self.step_index -= 1;
                        self.active = Self::build_step(&self.steps, self.step_index, &self.context);
                    } else {
                        self.push(SteppedSubmenuEvent::Cancel);
                    }
                }
                SelectSubmenuEvent::SelectionChanged(_) => {}
            }
        }
    }
}

impl Component for SteppedSubmenu {
    /// `settings-submenu.ts:157-159`.
    fn render(&self, width: usize) -> Vec<String> {
        self.active.render(width)
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        self.handle_key(data);
        Some(ComponentMutation { consume: true })
    }

    fn handle_mouse(
        &mut self,
        event: &pi_tui::mouse::MouseEvent,
    ) -> Option<pi_tui::mouse::MouseEventResult> {
        self.active.handle_mouse(event)
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

    fn queue() -> Arc<Mutex<Vec<SelectSubmenuEvent>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn options() -> Vec<SelectItem> {
        vec![
            SelectItem::new("dark", "Dark").with_description("Dark colors"),
            SelectItem::new("light", "Light").with_description("Light colors"),
            SelectItem::new("system", "System"),
        ]
    }

    fn plain(component: &impl Component, width: usize) -> Vec<String> {
        component
            .render(width)
            .iter()
            .map(|line| strip(line))
            .collect()
    }

    fn strip(line: &str) -> String {
        pi_tui::ansi::strip_terminal_sequences(line)
    }

    #[test]
    fn renders_title_description_list_and_hint() {
        let submenu =
            SelectSubmenu::new("Theme", Some("Pick one"), options(), Some("light"), queue());
        let lines = plain(&submenu, 60);
        assert_eq!(lines[0], "Theme");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "Pick one");
        assert!(lines.last().unwrap().contains("Esc to go back"));
        // The preselect landed on "light".
        let selected = lines.iter().find(|line| line.contains("→"));
        assert!(
            selected.is_some_and(|line| line.contains("Light")),
            "{lines:?}"
        );
    }

    #[test]
    fn enter_selects_and_escape_cancels() {
        let events = queue();
        let mut submenu = SelectSubmenu::new("Theme", None, options(), None, Arc::clone(&events));
        submenu.handle_key("\r");
        submenu.handle_key("\x1b[B"); // down to "light"
        submenu.handle_key("\r");
        assert_eq!(
            submenu.drain(),
            vec![
                SelectSubmenuEvent::Select("dark".into()),
                SelectSubmenuEvent::SelectionChanged("light".into()),
                SelectSubmenuEvent::Select("light".into()),
            ]
        );
        submenu.handle_key("\x1b");
        assert_eq!(submenu.drain(), vec![SelectSubmenuEvent::Cancel]);
    }

    #[test]
    fn search_filters_over_label_and_description() {
        let events = queue();
        let mut submenu =
            SelectSubmenu::new("Theme", None, options(), None, Arc::clone(&events)).with_search();
        // "dark" only matches the Dark option's label; "colors" matches both
        // described options through their descriptions.
        submenu.handle_key("colors");
        submenu.handle_key("\r");
        assert_eq!(
            submenu.drain(),
            vec![SelectSubmenuEvent::Select("dark".into())]
        );
    }

    #[test]
    fn nav_keys_reach_the_list_while_search_is_active() {
        let events = queue();
        let mut submenu =
            SelectSubmenu::new("Theme", None, options(), None, Arc::clone(&events)).with_search();
        submenu.handle_key("\x1b[B");
        assert_eq!(
            submenu.drain(),
            vec![SelectSubmenuEvent::SelectionChanged("light".into())]
        );
        assert_eq!(submenu.search_input.as_ref().unwrap().value(), "");
    }

    #[test]
    fn done_closes_with_the_selected_value() {
        let events = queue();
        let done = pi_tui::components::settings_list::SubmenuDone::new();
        let mut submenu =
            SelectSubmenu::new("Light Theme", None, options(), None, Arc::clone(&events))
                .with_done(done.clone());
        submenu.handle_key("\r");
        assert_eq!(
            submenu.drain(),
            vec![SelectSubmenuEvent::Select("dark".into())]
        );
        let close = done.take_close().expect("close");
        assert_eq!(close.value.as_deref(), Some("dark"));
        assert_eq!(close.navigate_to, None);
    }

    #[test]
    fn stepped_advances_backs_up_and_loops() {
        let events: Arc<Mutex<Vec<SteppedSubmenuEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let step = |key: &'static str| SubmenuStep {
            key,
            title: Box::new(|_| "Pick".to_string()),
            description: Box::new(|_| "A value".to_string()),
            options: Box::new(|_| vec![SelectItem::new("a", "A"), SelectItem::new("b", "B")]),
            preselect: None,
            searchable: false,
            layout: None,
        };
        let mut stepped = SteppedSubmenu::new(vec![step("model"), step("level")], true, events);
        // The step label shows on step 1 of 2.
        assert!(plain(&stepped, 60)[0].contains("Step 1/2"));
        stepped.handle_key("\r"); // model = a → advance
        assert!(plain(&stepped, 60)[0].contains("Step 2/2"));
        stepped.handle_key("\x1b"); // back
        assert!(plain(&stepped, 60)[0].contains("Step 1/2"));
        stepped.handle_key("\r"); // model = a
        stepped.handle_key("\x1b[B");
        stepped.handle_key("\r"); // level = b → complete + loop
        assert_eq!(
            stepped.drain(),
            vec![SteppedSubmenuEvent::Completed(SubmenuContext::from([
                ("model".to_string(), "a".to_string()),
                ("level".to_string(), "b".to_string()),
            ]))]
        );
        // Looped back to step 0 with a cleared context.
        assert!(plain(&stepped, 60)[0].contains("Step 1/2"));
        stepped.handle_key("\x1b");
        assert_eq!(stepped.drain(), vec![SteppedSubmenuEvent::Cancel]);
    }
}
