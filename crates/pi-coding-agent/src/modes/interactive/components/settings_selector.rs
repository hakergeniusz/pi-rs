//! `SettingsSelectorComponent` — the `/settings` dialog.
//!
//! Ground truth: `modes/interactive/components/settings-selector.js`. Only
//! the settings this port can apply end to end are listed — advertising
//! upstream's unimplemented rows would make the picker lie (auto-compact
//! waits on the agent-loop trigger, and the image, transport, cache,
//! telemetry, changelog, warnings, TUI-mode/fullscreen, padding, and
//! terminal-progress rows wait on their subsystems). The theme row is the
//! upstream `ThemeSubmenu` with preview and the `light/dark` automatic
//! setting.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::select_list::{SelectItem, SelectListLayout};
use pi_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListAction, SubmenuDone, SubmenuFactory,
};
use pi_tui::theme::ThemeColor;

use super::settings_submenu::{
    SelectSubmenu, SelectSubmenuEvent, SteppedSubmenu, SteppedSubmenuEvent, SubmenuContext,
    SubmenuStep,
};
use crate::modes::interactive::components::header::DynamicBorder;
use crate::modes::interactive::theme;

/// `CLEAR_OVERRIDE_VALUE` (`settings-selector.ts`).
const CLEAR_OVERRIDE_VALUE: &str = "__clear__";

/// `AUTOMATIC_THEME_VALUE` (`settings-selector.ts:81`).
const AUTOMATIC_THEME_VALUE: &str = "/";

/// `DEFAULT_PROJECT_TRUST_LABELS` (`settings-selector.ts:20-24`).
const DEFAULT_PROJECT_TRUST_LABELS: [(&str, &str); 3] = [
    ("ask", "Ask"),
    ("always", "Always trust"),
    ("never", "Never trust"),
];

/// `THINKING_DESCRIPTIONS` (`settings-selector.ts:11-19`).
fn thinking_description(level: &str) -> &'static str {
    match level {
        "off" => "No reasoning",
        "minimal" => "Very brief reasoning (~1k tokens)",
        "low" => "Light reasoning (~2k tokens)",
        "medium" => "Moderate reasoning (~8k tokens)",
        "high" => "Deep reasoning (~16k tokens)",
        "xhigh" => "Extra-high reasoning (~32k tokens)",
        "max" => "Maximum reasoning",
        _ => "",
    }
}

/// Events the selector publishes to the orchestrator (the `callbacks.*`
/// handlers of `showSettingsSelector`, `interactive-mode.ts:4227-4412`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsSelectorEvent {
    /// `onThemeChange` — the theme setting to persist and apply.
    ThemeChange(String),
    /// `onThemePreview` — live preview while navigating the theme menus.
    ThemePreview(String),
    /// `onSteeringModeChange`.
    SteeringModeChange(String),
    /// `onFollowUpModeChange`.
    FollowUpModeChange(String),
    /// `onHideThinkingBlockChange`.
    HideThinkingBlockChange(bool),
    /// `onModelThinkingLevelChange`.
    ModelThinkingLevelChange {
        provider: String,
        model_id: String,
        level: String,
    },
    /// `onModelThinkingLevelRemove`.
    ModelThinkingLevelRemove { provider: String, model_id: String },
    /// `onDefaultProjectTrustChange`.
    DefaultProjectTrustChange(String),
    /// `onTuiModeChange` — the mode to switch to; the orchestrator refuses
    /// while a selector occupies the editor slot.
    TuiModeChange(String),
    /// `onFullscreenExitOutputChange`.
    FullscreenExitOutputChange(String),
    /// `onFullscreenScrollbarChange`.
    FullscreenScrollbarChange(String),
    /// `onFullscreenCopyOnSelectChange`.
    FullscreenCopyOnSelectChange(bool),
    /// `onFullscreenWheelScrollLinesChange` — `"auto"` or a line count.
    FullscreenWheelScrollLinesChange(String),
    /// `onCancel`.
    Cancel,
}

/// The values the dialog renders, gathered from the orchestrator
/// (`showSettingsSelector`'s config object, `interactive-mode.ts:4184-4226`).
pub struct SettingsSelectorConfig {
    /// The current steering mode: `one-at-a-time` or `all`.
    pub steering_mode: String,
    /// The current follow-up mode.
    pub follow_up_mode: String,
    pub hide_thinking_block: bool,
    /// The global default thinking level, quoted by the clear-override hint.
    pub thinking_level: String,
    /// `provider/id` → level.
    pub model_thinking_levels: BTreeMap<String, String>,
    /// The credential-filtered models (`getAvailableSnapshot`).
    pub available_models: Vec<pi_ai::types::Model>,
    pub current_model: Option<(String, String)>,
    pub default_model: Option<(String, String)>,
    /// The theme setting (a name, or `light/dark` for automatic).
    pub current_theme: String,
    /// The terminal's reported appearance: `light` or `dark`.
    pub terminal_theme: String,
    /// `ask` | `always` | `never`.
    pub default_project_trust: String,
    /// The resolved TUI mode: `regular` or `fullscreen`.
    pub tui_mode: String,
    /// `transcript` or `resume-hint`.
    pub fullscreen_exit_output: String,
    /// `auto`, `always`, or `hidden`.
    pub fullscreen_scrollbar: String,
    /// Whether a selection copies itself when completed.
    pub fullscreen_copy_on_select: bool,
    /// `"auto"` or the resolved lines-per-wheel number.
    pub fullscreen_wheel_scroll_lines: String,
}

/// State the submenu factories mutate between steps: the live per-model
/// thinking overrides (upstream's `currentModelThinkingLevels` local).
struct SelectorState {
    model_thinking_levels: BTreeMap<String, String>,
}

/// `modelSettingKey`.
fn model_setting_key(provider: &str, id: &str) -> String {
    format!("{provider}/{id}")
}

/// `modelDisplayLabel`.
fn model_display_label(provider: &str, id: &str) -> String {
    format!("{id} [{provider}]")
}

/// `modelThinkingOverridesSummary`.
fn model_thinking_overrides_summary(overrides: &BTreeMap<String, String>) -> String {
    if overrides.is_empty() {
        "none".to_string()
    } else {
        format!("{} configured", overrides.len())
    }
}

/// `modelItemLabel`: the id with a muted provider bracket.
fn model_item_label(provider: &str, id: &str) -> String {
    format!(
        "{id} {}",
        theme::theme().fg(ThemeColor::Muted, &format!("[{provider}]"))
    )
}

/// `themeItems` (`settings-selector.ts:74-80`).
fn theme_items(available: &[String], current: &str) -> Vec<SelectItem> {
    available
        .iter()
        .map(|name| {
            let label = if name == current {
                format!("✓ {name}")
            } else {
                format!("  {name}")
            };
            let mut item = SelectItem::new(name.clone(), label);
            if name == pi_tui::theme::SYSTEM_THEME_NAME {
                item.description = Some("Theme created from your terminal's colors".to_string());
            }
            item
        })
        .collect()
}

/// `preferredTheme` (`settings-selector.ts:97-103`).
fn preferred_theme(available: &[String], preferred: Option<&str>, fallback: &str) -> String {
    if let Some(preferred) = preferred {
        if available.iter().any(|name| name == preferred) {
            return preferred.to_string();
        }
    }
    if available.iter().any(|name| name == fallback) {
        return fallback.to_string();
    }
    available
        .first()
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

/// `defaultAutomaticThemes` (`settings-selector.ts:104-111`).
fn default_automatic_themes(current_setting: &str, available: &[String]) -> (String, String) {
    if let Some(auto) = pi_tui::theme::parse_auto_theme_setting(Some(current_setting)) {
        return (auto.light_theme, auto.dark_theme);
    }
    let current_fixed = if current_setting.contains('/') {
        None
    } else {
        Some(current_setting)
    };
    let name = preferred_theme(available, current_fixed, pi_tui::theme::SYSTEM_THEME_NAME);
    (name.clone(), name)
}

/// Which view the theme submenu shows (`ThemeSubmenu.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeMenuMode {
    Single,
    Automatic,
}

/// The theme view currently mounted.
enum ThemeMenuView {
    Single(Box<SelectSubmenu>),
    Automatic(SettingsList),
}

/// `ThemeSubmenu` (`settings-selector.ts:112-252`): single-theme and
/// automatic light/dark menus with live previews. Events reach the selector
/// queue; `done` closes the settings-list row (a delivered value becomes the
/// row's new setting).
struct ThemeSubmenu {
    mode: ThemeMenuMode,
    single_theme: String,
    light_theme: String,
    dark_theme: String,
    available_themes: Vec<String>,
    terminal_theme: String,
    original_theme_setting: String,
    /// The selector's event queue.
    events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
    /// The inner selects' event queue (single menu + the light/dark pickers).
    select_events: Arc<Mutex<Vec<SelectSubmenuEvent>>>,
    done: SubmenuDone,
    active: ThemeMenuView,
}

impl ThemeSubmenu {
    /// `settings-selector.ts:123-143`.
    fn new(
        current_theme_setting: &str,
        terminal_theme: &str,
        available_themes: Vec<String>,
        events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
        done: SubmenuDone,
    ) -> Self {
        let auto = pi_tui::theme::parse_auto_theme_setting(Some(current_theme_setting));
        let (light_theme, dark_theme) =
            default_automatic_themes(current_theme_setting, &available_themes);
        let fixed_theme = if auto.is_some() || current_theme_setting.contains('/') {
            None
        } else {
            Some(current_theme_setting.to_string())
        };
        let mode = if auto.is_some() {
            ThemeMenuMode::Automatic
        } else {
            ThemeMenuMode::Single
        };
        let active_automatic = if terminal_theme == "light" {
            light_theme.clone()
        } else {
            dark_theme.clone()
        };
        let preferred = fixed_theme.or_else(|| {
            if auto.is_some() {
                Some(active_automatic.clone())
            } else {
                None
            }
        });
        let single_theme = preferred_theme(
            &available_themes,
            preferred.as_deref(),
            pi_tui::theme::SYSTEM_THEME_NAME,
        );
        let mut submenu = Self {
            mode,
            single_theme,
            light_theme,
            dark_theme,
            available_themes,
            terminal_theme: terminal_theme.to_string(),
            original_theme_setting: current_theme_setting.to_string(),
            events,
            select_events: Arc::new(Mutex::new(Vec::new())),
            done,
            active: ThemeMenuView::Single(Box::new(SelectSubmenu::new(
                "",
                None,
                Vec::new(),
                None,
                Arc::new(Mutex::new(Vec::new())),
            ))),
        };
        if submenu.mode == ThemeMenuMode::Automatic {
            submenu.show_automatic_menu();
        } else {
            submenu.show_single_menu();
        }
        submenu
    }

    /// `getActiveAutomaticTheme` (`settings-selector.ts:239-241`).
    fn get_active_automatic_theme(&self) -> String {
        if self.terminal_theme == "light" {
            self.light_theme.clone()
        } else {
            self.dark_theme.clone()
        }
    }

    /// `getThemeSetting` (`settings-selector.ts:236-238`).
    fn get_theme_setting(&self) -> String {
        match self.mode {
            ThemeMenuMode::Automatic => self.get_automatic_theme_setting(),
            ThemeMenuMode::Single => self.single_theme.clone(),
        }
    }

    /// `getAutomaticThemeSetting` (`settings-selector.ts:242-244`).
    fn get_automatic_theme_setting(&self) -> String {
        format!("{}/{}", self.light_theme, self.dark_theme)
    }

    /// `apply` (`settings-selector.ts:245-247`).
    fn apply(&self, theme_setting: &str) {
        self.done.close(Some(theme_setting), None);
    }

    /// `cancel` (`settings-selector.ts:248-251`).
    fn cancel(&self) {
        self.push(SettingsSelectorEvent::ThemePreview(
            self.original_theme_setting.clone(),
        ));
        self.done.close(None, None);
    }

    fn push(&self, event: SettingsSelectorEvent) {
        self.events.lock().expect("selector events").push(event);
    }

    /// `showSingleMenu` (`settings-selector.ts:152-167`). The select manages
    /// its own done timing: picking "automatic" switches menus instead of
    /// closing.
    fn show_single_menu(&mut self) {
        self.mode = ThemeMenuMode::Single;
        let items = single_mode_theme_items(&self.available_themes, &self.single_theme);
        let menu = SelectSubmenu::new(
            "Theme",
            Some("Select a theme, or choose automatic to follow terminal appearance."),
            items,
            Some(&self.single_theme),
            Arc::clone(&self.select_events),
        )
        .with_done_manual(self.done.clone());
        self.active = ThemeMenuView::Single(Box::new(menu));
    }

    /// `showAutomaticMenu` (`settings-selector.ts:168-229`).
    fn show_automatic_menu(&mut self) {
        self.mode = ThemeMenuMode::Automatic;
        let light_factory: SubmenuFactory = {
            let select_events = Arc::clone(&self.select_events);
            let available = self.available_themes.clone();
            Arc::new(move |current_value: &str, done: SubmenuDone| {
                Box::new(
                    SelectSubmenu::new(
                        "Light Theme",
                        Some("Select the theme to use for light terminal appearance"),
                        theme_items(&available, current_value),
                        Some(current_value),
                        Arc::clone(&select_events),
                    )
                    .with_done(done),
                ) as Box<dyn Component>
            })
        };
        let dark_factory: SubmenuFactory = {
            let select_events = Arc::clone(&self.select_events);
            let available = self.available_themes.clone();
            Arc::new(move |current_value: &str, done: SubmenuDone| {
                Box::new(
                    SelectSubmenu::new(
                        "Dark Theme",
                        Some("Select the theme to use for dark terminal appearance"),
                        theme_items(&available, current_value),
                        Some(current_value),
                        Arc::clone(&select_events),
                    )
                    .with_done(done),
                ) as Box<dyn Component>
            })
        };
        let items = vec![
            SettingItem::new("light-theme", "Light theme", self.light_theme.clone())
                .with_description("Theme to use in automatic mode when the terminal is light")
                .with_submenu(light_factory),
            SettingItem::new("dark-theme", "Dark theme", self.dark_theme.clone())
                .with_description("Theme to use in automatic mode when the terminal is dark")
                .with_submenu(dark_factory),
            SettingItem::new("apply", "Apply", "save and go back")
                .with_description("Save and go back")
                .with_values(vec!["save and go back".to_string()]),
            SettingItem::new("single-mode", "Change mode", "switch to single theme")
                .with_description("Switch to one theme for light and dark")
                .with_values(vec!["switch to single theme".to_string()]),
        ];
        self.active =
            ThemeMenuView::Automatic(SettingsList::new(items, 4, theme::settings_list_theme()));
    }

    /// React to a theme-picker selection delivered through the settings list
    /// (`createThemeSelect`'s onSelect, `settings-selector.ts:230-235`).
    fn handle_list_action(&mut self, action: SettingsListAction) {
        if let SettingsListAction::Changed { id, value } = action {
            match id.as_str() {
                "light-theme" => {
                    self.light_theme = value;
                    self.push(SettingsSelectorEvent::ThemePreview(
                        self.get_theme_setting(),
                    ));
                }
                "dark-theme" => {
                    self.dark_theme = value;
                    self.push(SettingsSelectorEvent::ThemePreview(
                        self.get_theme_setting(),
                    ));
                }
                "apply" => {
                    let setting = self.get_automatic_theme_setting();
                    self.apply(&setting);
                }
                "single-mode" => {
                    self.single_theme = self.get_active_automatic_theme();
                    self.push(SettingsSelectorEvent::ThemePreview(
                        self.single_theme.clone(),
                    ));
                    self.show_single_menu();
                }
                _ => {}
            }
        }
    }

    /// React to the single-mode select's events
    /// (`settings-selector.ts:154-165`).
    fn handle_single_select_event(&mut self, event: SelectSubmenuEvent) {
        match event {
            SelectSubmenuEvent::Select(value) => {
                if value == AUTOMATIC_THEME_VALUE {
                    self.push(SettingsSelectorEvent::ThemePreview(
                        self.get_theme_setting(),
                    ));
                    self.show_automatic_menu();
                } else {
                    self.single_theme = value;
                    let setting = self.single_theme.clone();
                    self.apply(&setting);
                }
            }
            SelectSubmenuEvent::SelectionChanged(value) => {
                let preview = if value == AUTOMATIC_THEME_VALUE {
                    self.get_automatic_theme_setting()
                } else {
                    value
                };
                self.push(SettingsSelectorEvent::ThemePreview(preview));
            }
            SelectSubmenuEvent::Cancel => self.cancel(),
        }
    }

    pub fn handle_key(&mut self, data: &str) {
        match &mut self.active {
            ThemeMenuView::Single(submenu) => {
                submenu.handle_key(data);
            }
            ThemeMenuView::Automatic(list) => {
                let action = list.handle_key(data);
                self.handle_list_action(action);
            }
        }
        // The single select and the light/dark pickers share one queue; the
        // active view decides what its events mean.
        for event in self.drain_select_events() {
            match self.mode {
                ThemeMenuMode::Single => self.handle_single_select_event(event),
                ThemeMenuMode::Automatic => match event {
                    // Selection previews while navigating a theme picker; a
                    // selection lands through the settings-list action; a
                    // cancel restores the preview.
                    SelectSubmenuEvent::SelectionChanged(value) => {
                        self.push(SettingsSelectorEvent::ThemePreview(value));
                    }
                    SelectSubmenuEvent::Cancel => {
                        self.push(SettingsSelectorEvent::ThemePreview(
                            self.get_theme_setting(),
                        ));
                    }
                    SelectSubmenuEvent::Select(_) => {}
                },
            }
        }
    }

    fn drain_select_events(&self) -> Vec<SelectSubmenuEvent> {
        std::mem::take(&mut *self.select_events.lock().expect("select events"))
    }
}

impl Component for ThemeSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        match &self.active {
            ThemeMenuView::Single(submenu) => submenu.render(width),
            ThemeMenuView::Automatic(list) => {
                // The heading container (`settings-selector.ts:170-175`).
                let theme = theme::theme();
                let mut lines = vec![
                    theme.fg(ThemeColor::Accent, &theme::bold("Automatic Theme")),
                    String::new(),
                    theme.fg(
                        ThemeColor::Muted,
                        "Choose themes for terminal light and dark appearance.",
                    ),
                    theme.fg(
                        ThemeColor::Muted,
                        "Light/dark detection requires terminal support.",
                    ),
                    String::new(),
                ];
                lines.extend(list.render(width));
                lines
            }
        }
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        self.handle_key(data);
        Some(ComponentMutation { consume: true })
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// `singleModeThemeItems` (`settings-selector.ts:83-96`): the system theme
/// first, then automatic mode, then the remaining themes.
fn single_mode_theme_items(available: &[String], current: &str) -> Vec<SelectItem> {
    let mut items: Vec<SelectItem> = theme_items(available, current);
    let system_index = items
        .iter()
        .position(|item| item.value == pi_tui::theme::SYSTEM_THEME_NAME);
    let mut ordered = Vec::new();
    if let Some(index) = system_index {
        ordered.push(items.remove(index));
    }
    ordered.push(
        SelectItem::new(AUTOMATIC_THEME_VALUE.to_string(), "  automatic".to_string())
            .with_description("Use separate themes for light and dark terminal appearance"),
    );
    ordered.extend(items);
    ordered
}

/// The stepped model-thinking submenu wrapper: the loop stays open, so the
/// only exit is escape, which closes with the updated summary
/// (`settings-selector.ts:469-484`).
struct ModelThinkingSubmenu {
    stepped: SteppedSubmenu,
    state: Arc<Mutex<SelectorState>>,
    selector_events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
    done: SubmenuDone,
}

impl ModelThinkingSubmenu {
    /// The two steps (`settings-selector.ts:396-467`).
    fn steps(
        state: &Arc<Mutex<SelectorState>>,
        available_models: &[pi_ai::types::Model],
        current_model_key: Option<String>,
        default_model_key: Option<String>,
        thinking_level: &str,
    ) -> Vec<SubmenuStep> {
        let preselect = current_model_key
            .clone()
            .or_else(|| default_model_key.clone());
        vec![
            model_step(
                state,
                available_models,
                current_model_key,
                default_model_key,
                preselect,
            ),
            level_step(state, thinking_level),
        ]
    }
}

/// The model chooser step (`settings-selector.ts:396-430`): available models
/// sorted current-first, default-second, each showing its override.
fn model_step(
    state: &Arc<Mutex<SelectorState>>,
    available_models: &[pi_ai::types::Model],
    current_model_key: Option<String>,
    default_model_key: Option<String>,
    preselect: Option<String>,
) -> SubmenuStep {
    let model_state = Arc::clone(state);
    let models = available_models.to_vec();
    SubmenuStep {
        key: "model",
        title: Box::new(|_| "Per-Model Thinking Level".to_string()),
        description: Box::new(|_| "Select a model to configure".to_string()),
        options: Box::new(move |_context| {
            let current_model_key = current_model_key.clone();
            let current_default_model_key = default_model_key.clone();
            let mut sorted = models.clone();
            sorted.sort_by(|a, b| {
                let rank = |model: &pi_ai::types::Model| {
                    let key = model_setting_key(&model.provider, &model.id);
                    if Some(&key) == current_model_key.as_ref() {
                        0
                    } else if Some(&key) == current_default_model_key.as_ref() {
                        1
                    } else {
                        2
                    }
                };
                rank(a)
                    .cmp(&rank(b))
                    .then_with(|| a.provider.cmp(&b.provider))
            });
            let state = model_state.lock().expect("selector state");
            let mut items: Vec<SelectItem> = sorted
                .iter()
                .map(|model| {
                    let key = model_setting_key(&model.provider, &model.id);
                    let mut item =
                        SelectItem::new(key.clone(), model_item_label(&model.provider, &model.id));
                    item.description = state.model_thinking_levels.get(&key).cloned();
                    item
                })
                .collect();
            if items.is_empty() {
                items.push(
                    SelectItem::new("__none__", "No models available")
                        .with_description("Log in to a provider or configure an API key first"),
                );
            }
            items
        }),
        preselect: Some(Box::new(move |_context| preselect.clone())),
        searchable: true,
        layout: Some(SelectListLayout {
            min_primary_column_width: Some(12),
            max_primary_column_width: Some(46),
        }),
    }
}

/// The level chooser step (`settings-selector.ts:432-467`): the model's
/// supported levels with the active one marked, plus a clear-override row
/// when an override exists.
fn level_step(state: &Arc<Mutex<SelectorState>>, thinking_level: &str) -> SubmenuStep {
    let level_state = Arc::clone(state);
    let level_preselect_state = Arc::clone(state);
    let thinking_level = thinking_level.to_string();
    SubmenuStep {
        key: "level",
        title: Box::new(|context: &SubmenuContext| {
            let key = context.get("model").cloned().unwrap_or_default();
            let (provider, id) = split_model_key(&key);
            let display = provider.zip(id).map_or_else(
                || key.clone(),
                |(provider, id)| model_display_label(&provider, &id),
            );
            format!("Thinking Level for {display}")
        }),
        description: Box::new(|_| "Select default thinking level for this model".to_string()),
        options: Box::new(move |context: &SubmenuContext| {
            let key = context.get("model").cloned().unwrap_or_default();
            let (provider, id) = split_model_key(&key);
            let Some((provider, id)) = provider.zip(id) else {
                return Vec::new();
            };
            let Some(model) = pi_ai::catalog::catalog()
                .models(&provider)
                .into_iter()
                .find(|model| model.id == id)
            else {
                return Vec::new();
            };
            let state = level_state.lock().expect("selector state");
            let active_level = state.model_thinking_levels.get(&key);
            let mut items: Vec<SelectItem> = pi_ai::types::get_supported_thinking_levels(model)
                .into_iter()
                .map(|level| {
                    let marker = if Some(level) == active_level.map(String::as_str) {
                        "✓ "
                    } else {
                        "  "
                    };
                    SelectItem::new(level, format!("{marker}{level}"))
                        .with_description(thinking_description(level))
                })
                .collect();
            if active_level.is_some() {
                items.push(
                    SelectItem::new(CLEAR_OVERRIDE_VALUE, "  (clear override)")
                        .with_description(format!("Revert to global default ({thinking_level})")),
                );
            }
            items
        }),
        preselect: Some(Box::new(move |context: &SubmenuContext| {
            let key = context.get("model")?;
            level_preselect_state
                .lock()
                .expect("selector state")
                .model_thinking_levels
                .get(key)
                .cloned()
        })),
        searchable: false,
        layout: None,
    }
}

/// `provider/id` → `(Some(provider), Some(id))` (both `None` when the key is
/// the `__none__` placeholder).
fn split_model_key(key: &str) -> (Option<String>, Option<String>) {
    match key.split_once('/') {
        Some((provider, id)) => (Some(provider.to_string()), Some(id.to_string())),
        None => (None, None),
    }
}

impl Component for ModelThinkingSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        self.stepped.render(width)
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        self.stepped.handle_input(data);
        for event in self.stepped.drain() {
            match event {
                SteppedSubmenuEvent::Completed(context) => {
                    // `onComplete` (`settings-selector.ts:469-483`).
                    let Some(key) = context.get("model").cloned() else {
                        continue;
                    };
                    let (provider, model_id) = split_model_key(&key);
                    let (Some(provider), Some(model_id)) = (provider, model_id) else {
                        continue;
                    };
                    let mut state = self.state.lock().expect("selector state");
                    match context.get("level").map(String::as_str) {
                        Some(CLEAR_OVERRIDE_VALUE) => {
                            state.model_thinking_levels.remove(&key);
                            drop(state);
                            self.selector_events.lock().expect("selector events").push(
                                SettingsSelectorEvent::ModelThinkingLevelRemove {
                                    provider,
                                    model_id,
                                },
                            );
                        }
                        Some(level) => {
                            state.model_thinking_levels.insert(key, level.to_string());
                            drop(state);
                            self.selector_events.lock().expect("selector events").push(
                                SettingsSelectorEvent::ModelThinkingLevelChange {
                                    provider,
                                    model_id,
                                    level: level.to_string(),
                                },
                            );
                        }
                        None => {}
                    }
                }
                SteppedSubmenuEvent::Cancel => {
                    // `onCancel` closes with the updated summary.
                    let summary = model_thinking_overrides_summary(
                        &self
                            .state
                            .lock()
                            .expect("selector state")
                            .model_thinking_levels,
                    );
                    self.done.close(Some(&summary), None);
                }
            }
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

/// The dialog (`SettingsSelectorComponent`, `settings-selector.ts:256-752`).
pub struct SettingsSelectorComponent {
    settings_list: SettingsList,
    events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
}

/// `fullscreenWheelScrollLines`'s row values (`settings-selector.ts:516-524`):
/// `"auto"`, then the presets and the current count deduplicated and sorted.
fn wheel_scroll_values(current: &str) -> Vec<String> {
    let mut values = vec!["auto".to_string()];
    let mut numbers: Vec<f64> = vec![1.0, 2.0, 3.0, 5.0, 10.0];
    if let Ok(lines) = current.parse::<f64>()
        && !numbers.contains(&lines)
    {
        numbers.push(lines);
    }
    numbers.sort_by(|a, b| a.partial_cmp(b).expect("finite line counts"));
    values.extend(numbers.iter().map(f64::to_string));
    values
}

/// The five fullscreen rows (`settings-selector.ts:489-525`), verbatim.
fn fullscreen_items(config: &SettingsSelectorConfig) -> Vec<SettingItem> {
    vec![
        SettingItem::new("tui-mode", "TUI mode", config.tui_mode.clone())
            .with_description(
                "Interface layout; regular mode uses the terminal's normal scrollback",
            )
            .with_values(vec!["regular".to_string(), "fullscreen".to_string()]),
        SettingItem::new(
            "fullscreen-exit-output",
            "Fullscreen exit output",
            config.fullscreen_exit_output.clone(),
        )
        .with_description(
            "Print the transcript or only a session resume hint when exiting \
             fullscreen mode",
        )
        .with_values(vec!["transcript".to_string(), "resume-hint".to_string()]),
        SettingItem::new(
            "fullscreen-scrollbar",
            "Fullscreen scrollbar",
            config.fullscreen_scrollbar.clone(),
        )
        .with_description("Scrollbar behavior in fullscreen mode; has no effect in regular mode")
        .with_values(vec![
            "auto".to_string(),
            "always".to_string(),
            "hidden".to_string(),
        ]),
        SettingItem::new(
            "fullscreen-copy-on-select",
            "Fullscreen copy on select",
            if config.fullscreen_copy_on_select {
                "true"
            } else {
                "false"
            },
        )
        .with_description(
            "Automatically copy selected text in fullscreen mode; disable to \
             copy selections with Ctrl+X",
        )
        .with_values(vec!["true".to_string(), "false".to_string()]),
        SettingItem::new(
            "fullscreen-wheel-scroll-lines",
            "Fullscreen wheel scrolling",
            config.fullscreen_wheel_scroll_lines.clone(),
        )
        .with_description(
            "Lines per mouse-wheel event in fullscreen mode; 'auto' speeds up \
             fast wheel spins where the terminal does not",
        )
        .with_values(wheel_scroll_values(&config.fullscreen_wheel_scroll_lines)),
    ]
}

/// The dialog rows (`settings-selector.ts:256-394`), in upstream order:
/// queue modes, thinking visibility, project trust, per-model thinking, the
/// fullscreen items, and the theme.
fn selector_items(
    config: &SettingsSelectorConfig,
    default_project_trust_label: &str,
    model_thinking_factory: SubmenuFactory,
    theme_factory: SubmenuFactory,
) -> Vec<SettingItem> {
    let follow_up_key = theme::key_text(theme::app_keys::MESSAGE_FOLLOW_UP);
    let cycle_thinking_key = theme::key_text(theme::app_keys::THINKING_CYCLE);
    let mut items = vec![
        SettingItem::new(
            "steering-mode",
            "Steering mode",
            config.steering_mode.clone(),
        )
        .with_description(
            "Enter while streaming queues steering messages. 'one-at-a-time': \
             deliver one, wait for response. 'all': deliver all at once.",
        )
        .with_values(vec!["one-at-a-time".to_string(), "all".to_string()]),
        SettingItem::new(
            "follow-up-mode",
            "Follow-up mode",
            config.follow_up_mode.clone(),
        )
        .with_description(format!(
            "{follow_up_key} queues follow-up messages until agent stops. \
             'one-at-a-time': deliver one, wait for response. 'all': deliver \
             all at once."
        ))
        .with_values(vec!["one-at-a-time".to_string(), "all".to_string()]),
        SettingItem::new(
            "hide-thinking",
            "Hide thinking",
            if config.hide_thinking_block {
                "true"
            } else {
                "false"
            },
        )
        .with_description("Hide thinking blocks in assistant responses")
        .with_values(vec!["true".to_string(), "false".to_string()]),
        SettingItem::new(
            "default-project-trust",
            "Default project trust",
            default_project_trust_label.to_string(),
        )
        .with_description(
            "Fallback behavior when no extension or saved trust decision \
             decides project trust",
        )
        .with_values(
            DEFAULT_PROJECT_TRUST_LABELS
                .iter()
                .map(|(_, label)| (*label).to_string())
                .collect(),
        ),
        SettingItem::new(
            "model-thinking",
            "Default thinking level per model",
            model_thinking_overrides_summary(&config.model_thinking_levels),
        )
        .with_description(format!(
            "Override the default thinking level for specific models. \
             {cycle_thinking_key} cycles in-session."
        ))
        .with_submenu(model_thinking_factory),
        SettingItem::new("theme", "Theme", config.current_theme.clone())
            .with_description("Color theme for the interface")
            .with_submenu(theme_factory),
    ];
    // The five fullscreen rows sit between the per-model thinking summary
    // and the theme (`settings-selector.ts:489-525`).
    let theme = items.pop().expect("the theme row");
    items.extend(fullscreen_items(config));
    items.push(theme);
    items
}

impl SettingsSelectorComponent {
    pub fn new(
        config: &SettingsSelectorConfig,
        events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
    ) -> Self {
        let state = Arc::new(Mutex::new(SelectorState {
            model_thinking_levels: config.model_thinking_levels.clone(),
        }));
        let default_project_trust_label = DEFAULT_PROJECT_TRUST_LABELS
            .iter()
            .find(|(value, _)| *value == config.default_project_trust)
            .map_or("Ask".to_string(), |(_, label)| (*label).to_string());
        let current_model_key = config
            .current_model
            .as_ref()
            .map(|(provider, id)| model_setting_key(provider, id));
        let default_model_key = config
            .default_model
            .as_ref()
            .map(|(provider, id)| model_setting_key(provider, id));

        let selector_events = Arc::clone(&events);
        let theme_factory: SubmenuFactory = {
            let terminal_theme = config.terminal_theme.clone();
            let available = theme::available_theme_names();
            Arc::new(move |current_value: &str, done: SubmenuDone| {
                Box::new(ThemeSubmenu::new(
                    current_value,
                    &terminal_theme,
                    available.clone(),
                    Arc::clone(&selector_events),
                    done,
                )) as Box<dyn Component>
            })
        };
        let selector_events = Arc::clone(&events);
        let thinking_state = Arc::clone(&state);
        let model_thinking_factory: SubmenuFactory = {
            let available_models = config.available_models.clone();
            let thinking_level = config.thinking_level.clone();
            Arc::new(move |_current_value: &str, done: SubmenuDone| {
                let stepped_events: Arc<Mutex<Vec<SteppedSubmenuEvent>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let stepped = SteppedSubmenu::new(
                    ModelThinkingSubmenu::steps(
                        &thinking_state,
                        &available_models,
                        current_model_key.clone(),
                        default_model_key.clone(),
                        &thinking_level,
                    ),
                    true,
                    Arc::clone(&stepped_events),
                );
                Box::new(ModelThinkingSubmenu {
                    stepped,
                    selector_events: Arc::clone(&selector_events),
                    state: Arc::clone(&thinking_state),
                    done,
                }) as Box<dyn Component>
            })
        };

        let items = selector_items(
            config,
            &default_project_trust_label,
            model_thinking_factory,
            theme_factory,
        );
        let settings_list =
            SettingsList::new(items, 10, theme::settings_list_theme()).with_search();
        Self {
            settings_list,
            events,
        }
    }

    pub fn settings_list(&mut self) -> &mut SettingsList {
        &mut self.settings_list
    }

    fn handle_list_action(&self, action: SettingsListAction) {
        match action {
            SettingsListAction::Changed { id, value } => match id.as_str() {
                "steering-mode" => {
                    self.push(SettingsSelectorEvent::SteeringModeChange(value));
                }
                "follow-up-mode" => {
                    self.push(SettingsSelectorEvent::FollowUpModeChange(value));
                }
                "hide-thinking" => {
                    self.push(SettingsSelectorEvent::HideThinkingBlockChange(
                        value == "true",
                    ));
                }
                "default-project-trust" => {
                    if let Some((key, _)) = DEFAULT_PROJECT_TRUST_LABELS
                        .iter()
                        .find(|(_, label)| *label == value)
                    {
                        self.push(SettingsSelectorEvent::DefaultProjectTrustChange(
                            (*key).to_string(),
                        ));
                    }
                }
                "tui-mode" => {
                    self.push(SettingsSelectorEvent::TuiModeChange(value));
                }
                "fullscreen-exit-output" => {
                    self.push(SettingsSelectorEvent::FullscreenExitOutputChange(value));
                }
                "fullscreen-scrollbar" => {
                    self.push(SettingsSelectorEvent::FullscreenScrollbarChange(value));
                }
                "fullscreen-copy-on-select" => {
                    self.push(SettingsSelectorEvent::FullscreenCopyOnSelectChange(
                        value == "true",
                    ));
                }
                "fullscreen-wheel-scroll-lines" => {
                    self.push(SettingsSelectorEvent::FullscreenWheelScrollLinesChange(
                        value,
                    ));
                }
                // The theme row's value is delivered by the submenu's done
                // callback; the summary updates of the model-thinking row
                // carry no event.
                "theme" => {
                    self.push(SettingsSelectorEvent::ThemeChange(value));
                }
                _ => {}
            },
            SettingsListAction::Cancelled => {
                self.push(SettingsSelectorEvent::Cancel);
            }
            _ => {}
        }
    }

    fn push(&self, event: SettingsSelectorEvent) {
        self.events.lock().expect("selector events").push(event);
    }

    /// Take the events published since the last drain.
    pub fn drain(&self) -> Vec<SettingsSelectorEvent> {
        std::mem::take(&mut *self.events.lock().expect("selector events"))
    }
}

impl Component for SettingsSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let border = DynamicBorder::new();
        let mut lines = Vec::new();
        lines.extend(border.render(0));
        lines.extend(self.settings_list.render(width));
        lines.extend(border.render(0));
        lines
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        let action = self.settings_list.handle_key(data);
        self.handle_list_action(action);
        Some(ComponentMutation { consume: true })
    }

    fn handle_mouse(
        &mut self,
        event: &pi_tui::mouse::MouseEvent,
    ) -> Option<pi_tui::mouse::MouseEventResult> {
        let result = self.settings_list.handle_mouse(event)?;
        if result.handled == Some(true) {
            let action = self.settings_list.take_action();
            self.handle_list_action(action);
        }
        Some(result)
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

    fn queue() -> Arc<Mutex<Vec<SettingsSelectorEvent>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn config() -> SettingsSelectorConfig {
        SettingsSelectorConfig {
            steering_mode: "one-at-a-time".to_string(),
            follow_up_mode: "one-at-a-time".to_string(),
            hide_thinking_block: false,
            thinking_level: "medium".to_string(),
            model_thinking_levels: BTreeMap::new(),
            available_models: Vec::new(),
            current_model: Some(("openai-codex".to_string(), "gpt-6-luna".to_string())),
            default_model: Some(("openai-codex".to_string(), "gpt-6-luna".to_string())),
            current_theme: "system".to_string(),
            terminal_theme: "dark".to_string(),
            default_project_trust: "ask".to_string(),
            tui_mode: "fullscreen".to_string(),
            fullscreen_exit_output: "transcript".to_string(),
            fullscreen_scrollbar: "auto".to_string(),
            fullscreen_copy_on_select: true,
            fullscreen_wheel_scroll_lines: "auto".to_string(),
        }
    }

    fn plain(selector: &SettingsSelectorComponent, width: usize) -> Vec<String> {
        selector
            .render(width)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect()
    }

    #[test]
    fn renders_the_scoped_rows_with_values_and_search() {
        let mut selector = SettingsSelectorComponent::new(&config(), queue());
        let lines = plain(&selector, 90);
        let joined = lines.join("\n");
        for label in [
            "Steering mode",
            "Follow-up mode",
            "Hide thinking",
            "Default project trust",
            "Default thinking level per model",
            "TUI mode",
        ] {
            assert!(joined.contains(label), "missing {label}: {joined}");
        }
        // The rows this port cannot apply are absent.
        assert!(!joined.contains("Auto-compact"), "{joined}");
        assert!(!joined.contains("Mermaid"), "{joined}");
        // The search row leads the list.
        assert!(lines.iter().any(|line| line.contains("Type to search")));
        assert!(joined.contains("Ask"), "{joined}");
        assert!(joined.contains("none"), "{joined}");
        // The ten-row window hides the tail until the selection scrolls.
        for _ in 0..10 {
            selector.handle_input("\x1b[B");
        }
        let tail = plain(&selector, 90).join("\n");
        assert!(
            tail.contains("Fullscreen wheel scrolling"),
            "missing the wheel row: {tail}"
        );
        assert!(tail.contains("Theme"), "missing the theme row: {tail}");
    }

    #[test]
    fn cycling_a_value_publishes_its_event() {
        let events = queue();
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        // First row is steering-mode.
        selector.handle_input("\r");
        assert_eq!(
            selector.drain(),
            vec![SettingsSelectorEvent::SteeringModeChange("all".to_string())]
        );
        selector.handle_input("\x1b");
        assert_eq!(selector.drain(), vec![SettingsSelectorEvent::Cancel]);
    }

    #[test]
    fn the_theme_submenu_previews_and_persists_through_done() {
        let events = queue();
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        // Navigate to the theme row (last of eleven) and open its submenu.
        for _ in 0..10 {
            selector.handle_input("\x1b[B");
        }
        selector.handle_input("\r");
        let lines = plain(&selector, 80);
        assert!(lines[1].contains("Theme"), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("automatic")));
        // Moving the selection previews; picking a theme delivers ThemeChange
        // through the done callback and closes the submenu. The single menu
        // holds system + automatic; down wraps to a real theme.
        selector.handle_input("\x1b[B");
        let drained = selector.drain();
        assert!(
            drained
                .iter()
                .any(|event| matches!(event, SettingsSelectorEvent::ThemePreview(_))),
            "{drained:?}"
        );
        selector.handle_input("\x1b[B");
        selector.handle_input("\r");
        let drained = selector.drain();
        assert!(
            drained
                .iter()
                .any(|event| matches!(event, SettingsSelectorEvent::ThemeChange(_))),
            "{drained:?}"
        );
        // Back on the main list (scrolled to the selected last row).
        let lines = plain(&selector, 80);
        assert!(lines.iter().any(|line| line.contains("Theme")), "{lines:?}");
    }

    #[test]
    fn the_automatic_theme_menu_applies_the_light_dark_setting() {
        let events = queue();
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        for _ in 0..10 {
            selector.handle_input("\x1b[B");
        }
        selector.handle_input("\r"); // open theme submenu
        selector.handle_input("\x1b[B"); // highlight "automatic"
        selector.handle_input("\r"); // switch to the automatic menu
        {
            let lines = plain(&selector, 80);
            assert!(lines[1].contains("Automatic Theme"), "{lines:?}");
        }
        // Pick the light theme ("system" is the only available one).
        selector.handle_input("\r");
        selector.handle_input("\r");
        // Navigate to "apply" and save the light/dark setting.
        selector.handle_input("\x1b[B");
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // "apply"
        let drained = selector.drain();
        let change = drained.iter().find_map(|event| match event {
            SettingsSelectorEvent::ThemeChange(setting) => Some(setting.clone()),
            _ => None,
        });
        assert!(
            change.is_some_and(|setting| setting.contains('/')),
            "{drained:?}"
        );
    }

    #[test]
    fn model_thinking_delivers_the_override_event() {
        let events = queue();
        let mut config = config();
        let model = pi_ai::catalog::catalog()
            .models("openai-codex")
            .into_iter()
            .find(|model| model.id == "gpt-6-luna")
            .expect("catalog model")
            .clone();
        config.available_models = vec![model.clone()];
        let mut selector = SettingsSelectorComponent::new(&config, events);
        // Row 4 (0-based): model-thinking.
        for _ in 0..4 {
            selector.handle_input("\x1b[B");
        }
        selector.handle_input("\r"); // open the stepped submenu
        {
            let lines = plain(&selector, 90);
            assert!(lines[1].contains("Step 1/2"), "{lines:?}");
            assert!(lines.iter().any(|line| line.contains("gpt-6-luna")));
        }
        selector.handle_input("\r"); // select the model
        {
            let lines = plain(&selector, 90);
            assert!(
                lines[1].contains("Thinking Level for gpt-6-luna"),
                "{lines:?}"
            );
        }
        selector.handle_input("\x1b[B"); // highlight "minimal"
        selector.handle_input("\r"); // select it → loop back to step 1
        assert_eq!(
            selector.drain(),
            vec![SettingsSelectorEvent::ModelThinkingLevelChange {
                provider: "openai-codex".to_string(),
                model_id: "gpt-6-luna".to_string(),
                level: "minimal".to_string(),
            }]
        );
        // The summary on the row updated for the next open.
        selector.handle_input("\x1b"); // escape closes with the summary
        let lines = plain(&selector, 90);
        assert!(
            lines.iter().any(|line| line.contains("1 configured")),
            "{lines:?}"
        );
        // Reopening shows the override in the model list description.
        selector.handle_input("\r");
        let lines = plain(&selector, 90);
        assert!(
            lines.iter().any(|line| line.contains("minimal")),
            "{lines:?}"
        );
        selector.handle_input("\r"); // select the model
        // The level list preselects the override ("minimal", index 1);
        // "(clear override)" sits after the model's supported levels.
        let level_count = pi_ai::types::get_supported_thinking_levels(&model).len();
        for _ in 0..(level_count - 1) {
            selector.handle_input("\x1b[B");
        }
        selector.handle_input("\r"); // "(clear override)"
        assert_eq!(
            selector.drain(),
            vec![SettingsSelectorEvent::ModelThinkingLevelRemove {
                provider: "openai-codex".to_string(),
                model_id: "gpt-6-luna".to_string(),
            }]
        );
    }

    #[test]
    fn default_project_trust_maps_labels_back_to_values() {
        let events = queue();
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        for _ in 0..3 {
            selector.handle_input("\x1b[B");
        }
        selector.handle_input("\r"); // cycle Ask → Always trust
        assert_eq!(
            selector.drain(),
            vec![SettingsSelectorEvent::DefaultProjectTrustChange(
                "always".to_string()
            )]
        );
    }

    /// The five fullscreen rows sit between the per-model thinking summary
    /// and the theme (`settings-selector.ts:489-525`), and each delivers its
    /// event with the row's raw value.
    #[test]
    fn the_fullscreen_rows_deliver_their_events() {
        let events = queue();
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        // Row 5 (0-based): tui-mode.
        for _ in 0..5 {
            selector.handle_input("\x1b[B");
        }
        selector.handle_input("\r"); // cycle fullscreen → regular
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // transcript → resume-hint
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // auto → always
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // true → false
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // auto → 1
        assert_eq!(
            selector.drain(),
            vec![
                SettingsSelectorEvent::TuiModeChange("regular".to_string()),
                SettingsSelectorEvent::FullscreenExitOutputChange("resume-hint".to_string()),
                SettingsSelectorEvent::FullscreenScrollbarChange("always".to_string()),
                SettingsSelectorEvent::FullscreenCopyOnSelectChange(false),
                SettingsSelectorEvent::FullscreenWheelScrollLinesChange("1".to_string()),
            ]
        );
    }

    /// The wheel row's values: `"auto"` first, then the presets and the
    /// current count deduplicated and sorted (`settings-selector.ts:516-524`).
    #[test]
    fn wheel_values_are_the_presets_plus_the_current_count() {
        assert_eq!(
            wheel_scroll_values("auto"),
            ["auto", "1", "2", "3", "5", "10"]
        );
        assert_eq!(
            wheel_scroll_values("7"),
            ["auto", "1", "2", "3", "5", "7", "10"]
        );
        assert_eq!(wheel_scroll_values("5"), ["auto", "1", "2", "3", "5", "10"]);
    }
}
