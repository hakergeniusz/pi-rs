//! `SettingsSelectorComponent` — the `/settings` dialog.
//!
//! Ground truth: `modes/interactive/components/settings-selector.ts` (1.5.2)
//! — the full main list (`:485-876`, the base literal plus the
//! image/hardware/editor splices), the value dispatch (`:881-1000`), the
//! `WarningSettingsSubmenu` (`:150-191`), the `ThemeSubmenu` (`:257-463`),
//! and the per-model thinking submenu (`:618-717`). Every upstream row is
//! listed in upstream order — the two-row-shorter shape when the terminal
//! cannot show inline images (`getCapabilities().images`, `:474`).
//!
//! Rows whose consumer is not ported yet persist their setting through the
//! [`crate::config::settings`] accessors only; the live-apply lands with the
//! row's subsystem. Auto-compact drives the compaction gate, the hardware
//! cursor / clear-on-shrink rows push into the TUI, the editor padding and
//! autocomplete rows push into the editor host, and output padding updates
//! the message rows; images, transport, cache warming, telemetry, changelog,
//! mermaid, warnings, and terminal progress persist only.
//!
//! Deviations:
//! * the "Session share destination" row is omitted — `/share` is not ported
//!   (ZIP-export-only policy), so the destination setting has no consumer.
//!   Upstream's row sits between "Transport" and "HTTP idle timeout"
//!   (`settings-selector.ts:515-522`).
//! * the numeric rows parse their controlled value lists with `parseInt`
//!   upstream and pass the number unchecked; the port parses into `i64` and
//!   lets the settings setters clamp.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::select_list::{SelectItem, SelectListLayout};
use pi_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListAction, SubmenuDone, SubmenuFactory,
};
use pi_tui::terminal_image::{ImageProtocol, get_capabilities};
use pi_tui::theme::ThemeColor;
use serde_json::{Map, Value};

use super::settings_submenu::{
    SelectSubmenu, SelectSubmenuEvent, SteppedSubmenu, SteppedSubmenuEvent, SubmenuContext,
    SubmenuStep,
};
use crate::config::settings::{
    CACHE_WARMING_MODES, DEFAULT_WARNINGS_ANTHROPIC_EXTRA_USAGE, HTTP_IDLE_TIMEOUT_CHOICES,
    MermaidRenderingMode, QuietStartup, format_http_idle_timeout_ms,
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
/// handlers of `showSettingsSelector`, `interactive-mode.ts:5097-5335`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsSelectorEvent {
    /// `onAutoCompactChange`.
    AutoCompactChange(bool),
    /// `onShowImagesChange`.
    ShowImagesChange(bool),
    /// `onImageWidthCellsChange` — the parsed width.
    ImageWidthCellsChange(i64),
    /// `onAutoResizeImagesChange`.
    AutoResizeImagesChange(bool),
    /// `onBlockImagesChange`.
    BlockImagesChange(bool),
    /// `onEnableSkillCommandsChange`.
    EnableSkillCommandsChange(bool),
    /// `onShowHardwareCursorChange`.
    ShowHardwareCursorChange(bool),
    /// `onEditorPaddingXChange` — the parsed padding.
    EditorPaddingXChange(i64),
    /// `onOutputPadChange` — `0` or `1`.
    OutputPadChange(i64),
    /// `onAutocompleteMaxVisibleChange` — the parsed maximum.
    AutocompleteMaxVisibleChange(i64),
    /// `onClearOnShrinkChange`.
    ClearOnShrinkChange(bool),
    /// `onShowTerminalProgressChange`.
    ShowTerminalProgressChange(bool),
    /// `onSteeringModeChange`.
    SteeringModeChange(String),
    /// `onFollowUpModeChange`.
    FollowUpModeChange(String),
    /// `onTransportChange` — the raw merged setting.
    TransportChange(String),
    /// `onHttpIdleTimeoutMsChange` — the timeout the chosen label stands for.
    HttpIdleTimeoutMsChange(i64),
    /// `onCacheWarmingModeChange`.
    CacheWarmingModeChange(String),
    /// `onHideThinkingBlockChange`.
    HideThinkingBlockChange(bool),
    /// `onMermaidRenderingModeChange`.
    MermaidRenderingModeChange(MermaidRenderingMode),
    /// `onShowCacheMissNoticesChange`.
    ShowCacheMissNoticesChange(bool),
    /// `onCollapseChangelogChange`.
    CollapseChangelogChange(bool),
    /// `onQuietStartupChange`.
    QuietStartupChange(QuietStartup),
    /// `onEnableInstallTelemetryChange`.
    EnableInstallTelemetryChange(bool),
    /// `onDefaultProjectTrustChange`.
    DefaultProjectTrustChange(String),
    /// `onDoubleEscapeActionChange`.
    DoubleEscapeActionChange(String),
    /// `onTreeFilterModeChange`.
    TreeFilterModeChange(String),
    /// `onWarningsChange` — the whole warnings object, unknown keys included.
    WarningsChange(Map<String, Value>),
    /// `onModelThinkingLevelChange`.
    ModelThinkingLevelChange {
        provider: String,
        model_id: String,
        level: String,
    },
    /// `onModelThinkingLevelRemove`.
    ModelThinkingLevelRemove { provider: String, model_id: String },
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
    /// `onThemeChange` — the theme setting to persist and apply.
    ThemeChange(String),
    /// `onThemePreview` — live preview while navigating the theme menus.
    ThemePreview(String),
    /// `onCancel`.
    Cancel,
}

/// The values the dialog renders, gathered from the orchestrator
/// (`showSettingsSelector`'s config object, `interactive-mode.ts:5103-5147`).
#[allow(clippy::struct_excessive_bools)] // mirrors the TS config object; split when extending
pub struct SettingsSelectorConfig {
    /// `autoCompact` — the session's auto-compaction gate
    /// (`session.autoCompactionEnabled` = `getCompactionEnabled`).
    pub auto_compact: bool,
    /// The current steering mode: `one-at-a-time` or `all`.
    pub steering_mode: String,
    /// The current follow-up mode.
    pub follow_up_mode: String,
    /// `transport` — the raw merged setting.
    pub transport: String,
    /// `httpIdleTimeoutMs` — the resolved timeout.
    pub http_idle_timeout_ms: i64,
    /// `cacheWarmingMode` — the global-only setting.
    pub cache_warming_mode: String,
    pub hide_thinking_block: bool,
    /// `markdown.mermaid`.
    pub mermaid_rendering_mode: String,
    pub show_cache_miss_notices: bool,
    pub collapse_changelog: bool,
    pub enable_install_telemetry: bool,
    /// `quietStartup` — `true`, `header`, or `false`.
    pub quiet_startup: String,
    /// `defaultProjectTrust` — `ask`, `always`, or `never`.
    pub default_project_trust: String,
    /// `doubleEscapeAction`.
    pub double_escape_action: String,
    /// `treeFilterMode`.
    pub tree_filter_mode: String,
    /// The global default thinking level, quoted by the clear-override hint.
    pub thinking_level: String,
    /// `provider/id` → level.
    pub model_thinking_levels: BTreeMap<String, String>,
    /// The credential-filtered models (`getAvailableSnapshot`).
    pub available_models: Vec<pi_ai::types::Model>,
    pub current_model: Option<(String, String)>,
    pub default_model: Option<(String, String)>,
    /// `terminal.showImages`.
    pub show_images: bool,
    /// `terminal.imageWidthCells`.
    pub image_width_cells: i64,
    /// `images.autoResize`.
    pub auto_resize_images: bool,
    /// `images.blockImages`.
    pub block_images: bool,
    /// `enableSkillCommands`.
    pub enable_skill_commands: bool,
    /// `showHardwareCursor`.
    pub show_hardware_cursor: bool,
    /// `editorPaddingX`.
    pub editor_padding_x: i64,
    /// `outputPad` — `0` or `1`.
    pub output_pad: i64,
    /// `autocompleteMaxVisible`.
    pub autocomplete_max_visible: i64,
    /// `terminal.clearOnShrink`.
    pub clear_on_shrink: bool,
    /// `terminal.showTerminalProgress`.
    pub show_terminal_progress: bool,
    /// The theme setting (a name, or `light/dark` for automatic).
    pub current_theme: String,
    /// The terminal's reported appearance: `light` or `dark`.
    pub terminal_theme: String,
    /// The warnings object, carried verbatim (`getWarnings`).
    pub warnings: Map<String, Value>,
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

/// State the submenu factories mutate between opens: the live per-model
/// thinking overrides (upstream's `currentModelThinkingLevels` local) and the
/// live warnings object (upstream's `currentWarnings` local,
/// `settings-selector.ts:477`).
struct SelectorState {
    model_thinking_levels: BTreeMap<String, String>,
    warnings: Map<String, Value>,
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

/// `themeItems` (`settings-selector.ts:213-219`).
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

/// `preferredTheme` (`settings-selector.ts:239-243`).
fn preferred_theme(available: &[String], preferred: Option<&str>, fallback: &str) -> String {
    if let Some(preferred) = preferred
        && available.iter().any(|name| name == preferred)
    {
        return preferred.to_string();
    }
    if available.iter().any(|name| name == fallback) {
        return fallback.to_string();
    }
    available
        .first()
        .cloned()
        .unwrap_or_else(|| fallback.to_string())
}

/// `defaultAutomaticThemes` (`settings-selector.ts:245-255`).
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

/// `ThemeSubmenu` (`settings-selector.ts:257-463`): single-theme and
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
    /// `settings-selector.ts:269-299`.
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

    /// `getActiveAutomaticTheme` (`settings-selector.ts:447-449`).
    fn get_active_automatic_theme(&self) -> String {
        if self.terminal_theme == "light" {
            self.light_theme.clone()
        } else {
            self.dark_theme.clone()
        }
    }

    /// `getThemeSetting` (`settings-selector.ts:443-445`).
    fn get_theme_setting(&self) -> String {
        match self.mode {
            ThemeMenuMode::Automatic => self.get_automatic_theme_setting(),
            ThemeMenuMode::Single => self.single_theme.clone(),
        }
    }

    /// `getAutomaticThemeSetting` (`settings-selector.ts:451-453`).
    fn get_automatic_theme_setting(&self) -> String {
        format!("{}/{}", self.light_theme, self.dark_theme)
    }

    /// `apply` (`settings-selector.ts:455-457`).
    fn apply(&self, theme_setting: &str) {
        self.done.close(Some(theme_setting), None);
    }

    /// `cancel` (`settings-selector.ts:459-462`).
    fn cancel(&self) {
        self.push(SettingsSelectorEvent::ThemePreview(
            self.original_theme_setting.clone(),
        ));
        self.done.close(None, None);
    }

    fn push(&self, event: SettingsSelectorEvent) {
        self.events.lock().expect("selector events").push(event);
    }

    /// `showSingleMenu` (`settings-selector.ts:311-335`). The select manages
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

    /// `showAutomaticMenu` (`settings-selector.ts:337-420`).
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
    /// (`createThemeSelect`'s onSelect, `settings-selector.ts:422-441`, and
    /// the automatic menu's `onChange`/`onCancel`, `:399-417`).
    fn handle_list_action(&mut self, action: SettingsListAction) {
        match action {
            SettingsListAction::Changed { id, value } => match id.as_str() {
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
            },
            // The automatic menu's `onCancel` (`settings-selector.ts:416`).
            SettingsListAction::Cancelled => self.cancel(),
            _ => {}
        }
    }

    /// React to the single-mode select's events
    /// (`settings-selector.ts:318-332`).
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
                // The heading container (`settings-selector.ts:339-344`).
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

/// `singleModeThemeItems` (`settings-selector.ts:224-237`): the system theme
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

/// `WarningSettingsSubmenu` (`settings-selector.ts:150-191`): a one-row
/// settings list over the shared warnings object. A change publishes the
/// whole object (`onChange` → `onWarningsChange`); escape closes through the
/// row's done.
struct WarningSettingsSubmenu {
    settings_list: SettingsList,
    state: Arc<Mutex<SelectorState>>,
    events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
    done: SubmenuDone,
}

impl WarningSettingsSubmenu {
    /// `settings-selector.ts:154-186`.
    fn new(
        state: &Arc<Mutex<SelectorState>>,
        events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
        done: SubmenuDone,
    ) -> Self {
        let current = state
            .lock()
            .expect("selector state")
            .warnings
            .get("anthropicExtraUsage")
            .and_then(Value::as_bool)
            .unwrap_or(DEFAULT_WARNINGS_ANTHROPIC_EXTRA_USAGE);
        let items = vec![
            SettingItem::new(
                "anthropic-extra-usage",
                "Anthropic extra usage",
                if current { "true" } else { "false" },
            )
            .with_description("Warn when Anthropic subscription auth may use paid extra usage")
            .with_values(vec!["true".to_string(), "false".to_string()]),
        ];
        Self {
            settings_list: SettingsList::new(items, 1, theme::settings_list_theme()),
            state: Arc::clone(state),
            events,
            done,
        }
    }

    fn handle_key(&mut self, data: &str) {
        match self.settings_list.handle_key(data) {
            SettingsListAction::Changed { id, value } if id == "anthropic-extra-usage" => {
                // `this.state = { ...this.state, anthropicExtraUsage: … }`
                // (`settings-selector.ts:174-181`): the unknown keys ride
                // along, exactly like the TS spread.
                let warnings = {
                    let mut state = self.state.lock().expect("selector state");
                    state.warnings.insert(
                        "anthropicExtraUsage".to_string(),
                        Value::Bool(value == "true"),
                    );
                    state.warnings.clone()
                };
                self.events
                    .lock()
                    .expect("selector events")
                    .push(SettingsSelectorEvent::WarningsChange(warnings));
            }
            // `onCancel` (`settings-selector.ts:183`).
            SettingsListAction::Cancelled => self.done.close(None, None),
            _ => {}
        }
    }
}

impl Component for WarningSettingsSubmenu {
    fn render(&self, width: usize) -> Vec<String> {
        self.settings_list.render(width)
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

/// The stepped model-thinking submenu wrapper: the loop stays open, so the
/// only exit is escape, which closes with the updated summary
/// (`settings-selector.ts:692-716`).
struct ModelThinkingSubmenu {
    stepped: SteppedSubmenu,
    state: Arc<Mutex<SelectorState>>,
    selector_events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
    done: SubmenuDone,
}

impl ModelThinkingSubmenu {
    /// The two steps (`settings-selector.ts:623-690`).
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

/// The model chooser step (`settings-selector.ts:624-659`): available models
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

/// The level chooser step (`settings-selector.ts:660-690`): the model's
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
            let Some(model) = crate::models_state::models_for(&provider)
                .into_iter()
                .find(|model| model.id == id)
            else {
                return Vec::new();
            };
            let state = level_state.lock().expect("selector state");
            let active_level = state.model_thinking_levels.get(&key);
            let mut items: Vec<SelectItem> = pi_ai::types::get_supported_thinking_levels(&model)
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
                    // `onComplete` (`settings-selector.ts:696-710`).
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

/// The dialog (`SettingsSelectorComponent`, `settings-selector.ts:468-1009`).
pub struct SettingsSelectorComponent {
    settings_list: SettingsList,
    events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
}

/// `getCapabilities().images` (`settings-selector.ts:474`): the image rows
/// exist only when the terminal can render inline images.
fn supports_images() -> bool {
    get_capabilities().images != ImageProtocol::Disabled
}

/// `parseInt(newValue, 10)` (`settings-selector.ts:894,964,970`). The row
/// values are the controlled numeric strings above, so a parse failure
/// cannot happen; `0` keeps it from panicking if one ever does.
fn parse_int(value: &str) -> i64 {
    value.trim().parse::<i64>().unwrap_or(0)
}

/// `fullscreenWheelScrollLines`'s row values (`settings-selector.ts:747-758`):
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

/// The fullscreen rows (`settings-selector.ts:719-759`), verbatim.
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

/// `SettingItem::new` with the boolean rows' shared shape: `true`/`false`
/// values over the current flag (`settings-selector.ts` passim).
fn bool_item(id: &str, label: &str, description: &str, current: bool) -> SettingItem {
    SettingItem::new(id, label, if current { "true" } else { "false" })
        .with_description(description)
        .with_values(vec!["true".to_string(), "false".to_string()])
}

/// The dialog rows (`settings-selector.ts:485-876`), in upstream order: the
/// base literal first (`:485-768`), then the image/cursor/editor/padding/
/// terminal splices (`:770-876`) inserted where upstream's `splice` calls
/// put them. The `"Session share destination"` row is omitted — see the
/// module deviation note.
#[allow(clippy::too_many_lines)] // mirrors the TS item literal; split when extending
fn selector_items(
    config: &SettingsSelectorConfig,
    default_project_trust_label: &str,
    warnings_factory: SubmenuFactory,
    model_thinking_factory: SubmenuFactory,
    theme_factory: SubmenuFactory,
) -> Vec<SettingItem> {
    let follow_up_key = theme::key_text(theme::app_keys::MESSAGE_FOLLOW_UP);
    let cycle_thinking_key = theme::key_text(theme::app_keys::THINKING_CYCLE);
    let mut items = vec![
        // `settings-selector.ts:486-492`.
        bool_item(
            "autocompact",
            "Auto-compact",
            "Automatically compact context when it gets too large",
            config.auto_compact,
        ),
    ];
    if supports_images() {
        // The image rows splice in after autocompact
        // (`settings-selector.ts:771-787`).
        items.push(bool_item(
            "show-images",
            "Show images",
            "Render images inline in terminal",
            config.show_images,
        ));
        items.push(
            SettingItem::new(
                "image-width-cells",
                "Image width",
                config.image_width_cells.to_string(),
            )
            .with_description("Preferred inline image width in terminal cells")
            .with_values(vec!["60".to_string(), "80".to_string(), "120".to_string()]),
        );
    }
    // `settings-selector.ts:790-796`.
    items.push(bool_item(
        "auto-resize-images",
        "Auto-resize images",
        "Resize large images to 2000x2000 max for better model compatibility",
        config.auto_resize_images,
    ));
    // `settings-selector.ts:799-806`.
    items.push(bool_item(
        "block-images",
        "Block images",
        "Prevent images from being sent to LLM providers",
        config.block_images,
    ));
    // `settings-selector.ts:809-816`.
    items.push(bool_item(
        "skill-commands",
        "Skill commands",
        "Register skills as /skill:name commands",
        config.enable_skill_commands,
    ));
    // `settings-selector.ts:819-826`.
    items.push(bool_item(
        "show-hardware-cursor",
        "Show hardware cursor",
        "Use the terminal cursor instead of Pi's drawn cursor",
        config.show_hardware_cursor,
    ));
    // `settings-selector.ts:829-836`.
    items.push(
        SettingItem::new(
            "editor-padding",
            "Editor padding",
            config.editor_padding_x.to_string(),
        )
        .with_description("Horizontal padding for input editor (0-3)")
        .with_values(
            ["0", "1", "2", "3"]
                .iter()
                .map(|v| (*v).to_string())
                .collect(),
        ),
    );
    // `settings-selector.ts:839-846`.
    items.push(
        SettingItem::new(
            "output-padding",
            "Output padding",
            config.output_pad.to_string(),
        )
        .with_description("Horizontal padding for messages, tool output, and command output")
        .with_values(vec!["0".to_string(), "1".to_string()]),
    );
    // `settings-selector.ts:849-856`.
    items.push(
        SettingItem::new(
            "autocomplete-max-visible",
            "Autocomplete max items",
            config.autocomplete_max_visible.to_string(),
        )
        .with_description("Max visible items in autocomplete dropdown (3-20)")
        .with_values(
            ["3", "5", "7", "10", "15", "20"]
                .iter()
                .map(|v| (*v).to_string())
                .collect(),
        ),
    );
    // `settings-selector.ts:859-866`.
    items.push(bool_item(
        "clear-on-shrink",
        "Clear on shrink",
        "Clear empty rows when content shrinks (may cause flicker)",
        config.clear_on_shrink,
    ));
    // `settings-selector.ts:869-876`.
    items.push(bool_item(
        "terminal-progress",
        "Terminal progress",
        "Show OSC 9;4 progress indicators in the terminal tab bar",
        config.show_terminal_progress,
    ));
    // The rest of the base literal (`settings-selector.ts:494-768`).
    items.push(
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
    );
    items.push(
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
    );
    items.push(
        SettingItem::new("transport", "Transport", config.transport.clone())
            .with_description("Preferred transport for providers that support multiple transports")
            .with_values(vec![
                "sse".to_string(),
                "websocket".to_string(),
                "websocket-cached".to_string(),
                "auto".to_string(),
            ]),
    );
    // deviation: the "Session share destination" row
    // (`settings-selector.ts:515-522`) is omitted — /share is not ported
    // (ZIP-export-only policy), so the setting has no consumer.
    items.push(
        SettingItem::new(
            "http-idle-timeout",
            "HTTP idle timeout",
            format_http_idle_timeout_ms(config.http_idle_timeout_ms),
        )
        .with_description(
            "Maximum idle gap while waiting for HTTP headers or body chunks. \
             Disable for local models that pause longer than five minutes.",
        )
        .with_values(
            HTTP_IDLE_TIMEOUT_CHOICES
                .iter()
                .map(|(label, _)| (*label).to_string())
                .collect(),
        ),
    );
    items.push(
        SettingItem::new(
            "cache-warming-mode",
            "Cache warming",
            config.cache_warming_mode.clone(),
        )
        .with_description(
            "off; streaming while the agent runs; idle also between runs \
             while continuation stays profitable",
        )
        .with_values(
            CACHE_WARMING_MODES
                .iter()
                .map(|mode| (*mode).to_string())
                .collect(),
        ),
    );
    items.push(bool_item(
        "hide-thinking",
        "Hide thinking",
        "Hide thinking blocks in assistant responses",
        config.hide_thinking_block,
    ));
    items.push(
        SettingItem::new(
            "mermaid-rendering",
            "Mermaid diagrams",
            config.mermaid_rendering_mode.clone(),
        )
        .with_description("Render Mermaid code blocks as Unicode diagrams")
        .with_values(vec![
            "off".to_string(),
            "final".to_string(),
            "streaming".to_string(),
        ]),
    );
    items.push(bool_item(
        "cache-miss-notices",
        "Cache miss notices",
        "Show transcript notices for cache costs and provider recovery diagnostics",
        config.show_cache_miss_notices,
    ));
    items.push(bool_item(
        "collapse-changelog",
        "Collapse changelog",
        "Show condensed changelog after updates",
        config.collapse_changelog,
    ));
    items.push(
        SettingItem::new(
            "quiet-startup",
            "Quiet startup",
            config.quiet_startup.clone(),
        )
        .with_description(
            "Disable verbose printing at startup (header: keep only the \
             startup header)",
        )
        .with_values(vec![
            "true".to_string(),
            "header".to_string(),
            "false".to_string(),
        ]),
    );
    items.push(bool_item(
        "install-telemetry",
        "Install telemetry",
        "Send an anonymous version/update ping after changelog-detected updates",
        config.enable_install_telemetry,
    ));
    items.push(
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
    );
    items.push(
        SettingItem::new(
            "double-escape-action",
            "Double-escape action",
            config.double_escape_action.clone(),
        )
        .with_description("Action when pressing Escape twice with empty editor")
        .with_values(vec![
            "tree".to_string(),
            "fork".to_string(),
            "none".to_string(),
        ]),
    );
    items.push(
        SettingItem::new(
            "tree-filter-mode",
            "Tree filter mode",
            config.tree_filter_mode.clone(),
        )
        .with_description("Default filter when opening /tree")
        .with_values(vec![
            "default".to_string(),
            "no-tools".to_string(),
            "user-only".to_string(),
            "labeled-only".to_string(),
            "all".to_string(),
        ]),
    );
    items.push(
        SettingItem::new("warnings", "Warnings", "configure")
            .with_description("Enable or disable individual warnings")
            .with_submenu(warnings_factory),
    );
    items.push(
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
    );
    items.extend(fullscreen_items(config));
    items.push(
        SettingItem::new("theme", "Theme", config.current_theme.clone())
            .with_description("Color theme for the interface")
            .with_submenu(theme_factory),
    );
    items
}

impl SettingsSelectorComponent {
    pub fn new(
        config: &SettingsSelectorConfig,
        events: Arc<Mutex<Vec<SettingsSelectorEvent>>>,
    ) -> Self {
        let state = Arc::new(Mutex::new(SelectorState {
            model_thinking_levels: config.model_thinking_levels.clone(),
            warnings: config.warnings.clone(),
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
        let warnings_state = Arc::clone(&state);
        let warnings_factory: SubmenuFactory =
            Arc::new(move |_current_value: &str, done: SubmenuDone| {
                Box::new(WarningSettingsSubmenu::new(
                    &warnings_state,
                    Arc::clone(&selector_events),
                    done,
                )) as Box<dyn Component>
            });
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
            warnings_factory,
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

    #[allow(clippy::too_many_lines)] // mirrors the TS onChange switch; split when extending
    fn handle_list_action(&self, action: SettingsListAction) {
        match action {
            SettingsListAction::Changed { id, value } => match id.as_str() {
                "autocompact" => {
                    self.push(SettingsSelectorEvent::AutoCompactChange(value == "true"));
                }
                "show-images" => {
                    self.push(SettingsSelectorEvent::ShowImagesChange(value == "true"));
                }
                "image-width-cells" => {
                    self.push(SettingsSelectorEvent::ImageWidthCellsChange(parse_int(
                        &value,
                    )));
                }
                "auto-resize-images" => {
                    self.push(SettingsSelectorEvent::AutoResizeImagesChange(
                        value == "true",
                    ));
                }
                "block-images" => {
                    self.push(SettingsSelectorEvent::BlockImagesChange(value == "true"));
                }
                "skill-commands" => {
                    self.push(SettingsSelectorEvent::EnableSkillCommandsChange(
                        value == "true",
                    ));
                }
                "show-hardware-cursor" => {
                    self.push(SettingsSelectorEvent::ShowHardwareCursorChange(
                        value == "true",
                    ));
                }
                "editor-padding" => {
                    self.push(SettingsSelectorEvent::EditorPaddingXChange(parse_int(
                        &value,
                    )));
                }
                "output-padding" => {
                    // `newValue === "0" ? 0 : 1`
                    // (`settings-selector.ts:966-968`).
                    let padding = i64::from(value != "0");
                    self.push(SettingsSelectorEvent::OutputPadChange(padding));
                }
                "autocomplete-max-visible" => {
                    self.push(SettingsSelectorEvent::AutocompleteMaxVisibleChange(
                        parse_int(&value),
                    ));
                }
                "clear-on-shrink" => {
                    self.push(SettingsSelectorEvent::ClearOnShrinkChange(value == "true"));
                }
                "terminal-progress" => {
                    self.push(SettingsSelectorEvent::ShowTerminalProgressChange(
                        value == "true",
                    ));
                }
                "steering-mode" => {
                    self.push(SettingsSelectorEvent::SteeringModeChange(value));
                }
                "follow-up-mode" => {
                    self.push(SettingsSelectorEvent::FollowUpModeChange(value));
                }
                "transport" => {
                    self.push(SettingsSelectorEvent::TransportChange(value));
                }
                "http-idle-timeout" => {
                    // The label → choice lookup
                    // (`settings-selector.ts:917-923`).
                    if let Some((_, timeout_ms)) = HTTP_IDLE_TIMEOUT_CHOICES
                        .iter()
                        .find(|(label, _)| *label == value)
                    {
                        self.push(SettingsSelectorEvent::HttpIdleTimeoutMsChange(*timeout_ms));
                    }
                }
                "cache-warming-mode" => {
                    self.push(SettingsSelectorEvent::CacheWarmingModeChange(value));
                }
                "hide-thinking" => {
                    self.push(SettingsSelectorEvent::HideThinkingBlockChange(
                        value == "true",
                    ));
                }
                "mermaid-rendering" => {
                    let mode = match value.as_str() {
                        "off" => MermaidRenderingMode::Off,
                        "final" => MermaidRenderingMode::Final,
                        _ => MermaidRenderingMode::Streaming,
                    };
                    self.push(SettingsSelectorEvent::MermaidRenderingModeChange(mode));
                }
                "cache-miss-notices" => {
                    self.push(SettingsSelectorEvent::ShowCacheMissNoticesChange(
                        value == "true",
                    ));
                }
                "collapse-changelog" => {
                    self.push(SettingsSelectorEvent::CollapseChangelogChange(
                        value == "true",
                    ));
                }
                "quiet-startup" => {
                    // `newValue === "header" ? "header" : newValue === "true"`
                    // (`settings-selector.ts:939-941`).
                    let quiet = match value.as_str() {
                        "header" => QuietStartup::Header,
                        "true" => QuietStartup::Quiet,
                        _ => QuietStartup::Off,
                    };
                    self.push(SettingsSelectorEvent::QuietStartupChange(quiet));
                }
                "install-telemetry" => {
                    self.push(SettingsSelectorEvent::EnableInstallTelemetryChange(
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
                "double-escape-action" => {
                    self.push(SettingsSelectorEvent::DoubleEscapeActionChange(value));
                }
                "tree-filter-mode" => {
                    self.push(SettingsSelectorEvent::TreeFilterModeChange(value));
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
    use pi_tui::terminal_image::{TerminalCapabilities, set_capabilities};

    fn queue() -> Arc<Mutex<Vec<SettingsSelectorEvent>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    /// Force the image rows off. The capability cache is thread-local and the
    /// test harness runs each test on its own thread, so this pins the row
    /// shape per test.
    fn disable_images() {
        set_capabilities(TerminalCapabilities {
            images: ImageProtocol::Disabled,
            true_color: true,
            hyperlinks: false,
        });
    }

    fn config() -> SettingsSelectorConfig {
        let config = SettingsSelectorConfig {
            auto_compact: true,
            steering_mode: "one-at-a-time".to_string(),
            follow_up_mode: "one-at-a-time".to_string(),
            transport: "auto".to_string(),
            http_idle_timeout_ms: 300_000,
            cache_warming_mode: "idle".to_string(),
            hide_thinking_block: false,
            mermaid_rendering_mode: "streaming".to_string(),
            show_cache_miss_notices: false,
            collapse_changelog: false,
            enable_install_telemetry: true,
            quiet_startup: "false".to_string(),
            default_project_trust: "ask".to_string(),
            double_escape_action: "tree".to_string(),
            tree_filter_mode: "default".to_string(),
            thinking_level: "medium".to_string(),
            model_thinking_levels: BTreeMap::new(),
            available_models: Vec::new(),
            current_model: Some(("openai-codex".to_string(), "gpt-6-luna".to_string())),
            default_model: Some(("openai-codex".to_string(), "gpt-6-luna".to_string())),
            show_images: true,
            image_width_cells: 60,
            auto_resize_images: true,
            block_images: false,
            enable_skill_commands: true,
            show_hardware_cursor: false,
            editor_padding_x: 0,
            output_pad: 1,
            autocomplete_max_visible: 5,
            clear_on_shrink: false,
            show_terminal_progress: false,
            current_theme: "system".to_string(),
            terminal_theme: "dark".to_string(),
            warnings: Map::new(),
            tui_mode: "fullscreen".to_string(),
            fullscreen_exit_output: "transcript".to_string(),
            fullscreen_scrollbar: "auto".to_string(),
            fullscreen_copy_on_select: true,
            fullscreen_wheel_scroll_lines: "auto".to_string(),
        };
        disable_images();
        config
    }

    fn plain(component: &impl Component, width: usize) -> Vec<String> {
        component
            .render(width)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect()
    }

    /// The row labels in render order, read off the rendered list's selected
    /// row (`{cursor}{label padded}  {value}`), walking down until the list
    /// wraps. The port must present upstream's list in upstream's order.
    fn rendered_labels(selector: &mut SettingsSelectorComponent) -> Vec<String> {
        let mut labels = Vec::new();
        loop {
            let lines = plain(selector, 100);
            let row = lines.iter().find(|line| line.contains("→ "));
            let Some(row) = row else {
                break;
            };
            let after_cursor = row.split("→ ").last().unwrap_or_default().trim_start();
            // The label column pads to the widest label, so the value column
            // always starts after two or more spaces.
            let Some((label, _)) = after_cursor.split_once("  ") else {
                break;
            };
            // Down stops at the wrap-around: the first already-seen label
            // means the list is fully captured.
            if labels.contains(&label.to_string()) {
                break;
            }
            labels.push(label.to_string());
            selector.handle_input("\x1b[B");
            if labels.len() > 64 {
                break;
            }
        }
        labels
    }

    #[test]
    fn renders_the_full_upstream_list_in_order() {
        let mut selector = SettingsSelectorComponent::new(&config(), queue());
        let labels = rendered_labels(&mut selector);
        assert_eq!(
            labels,
            vec![
                "Auto-compact",
                "Auto-resize images",
                "Block images",
                "Skill commands",
                "Show hardware cursor",
                "Editor padding",
                "Output padding",
                "Autocomplete max items",
                "Clear on shrink",
                "Terminal progress",
                "Steering mode",
                "Follow-up mode",
                "Transport",
                "HTTP idle timeout",
                "Cache warming",
                "Hide thinking",
                "Mermaid diagrams",
                "Cache miss notices",
                "Collapse changelog",
                "Quiet startup",
                "Install telemetry",
                "Default project trust",
                "Double-escape action",
                "Tree filter mode",
                "Warnings",
                "Default thinking level per model",
                "TUI mode",
                "Fullscreen exit output",
                "Fullscreen scrollbar",
                "Fullscreen copy on select",
                "Fullscreen wheel scrolling",
                "Theme",
            ],
            "row order or labels drifted: {labels:?}"
        );
        // The search row leads the list (`enableSearch: true`,
        // `settings-selector.ts:999`).
        let lines = plain(&selector, 90);
        assert!(lines.iter().any(|line| line.contains("Type to search")));
    }

    #[test]
    fn image_support_inserts_the_image_rows_after_autocompact() {
        let config = config();
        set_capabilities(TerminalCapabilities {
            images: ImageProtocol::Kitty,
            true_color: true,
            hyperlinks: false,
        });
        let mut selector = SettingsSelectorComponent::new(&config, queue());
        let labels = rendered_labels(&mut selector);
        let positions = |label: &str| {
            labels
                .iter()
                .position(|candidate| candidate == label)
                .unwrap_or_else(|| panic!("missing {label}: {labels:?}"))
        };
        assert_eq!(positions("Auto-compact") + 1, positions("Show images"));
        assert_eq!(positions("Show images") + 1, positions("Image width"));
        assert_eq!(
            positions("Image width") + 1,
            positions("Auto-resize images")
        );
        assert_eq!(labels.len(), 34, "two extra rows with images: {labels:?}");
        // The capability cache is thread-local and the harness runs each test
        // on its own thread, so no reset is needed.
    }

    #[test]
    fn cycling_a_value_publishes_its_event() {
        let events = queue();
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        // First row is autocompact.
        selector.handle_input("\r");
        assert_eq!(
            selector.drain(),
            vec![SettingsSelectorEvent::AutoCompactChange(false)]
        );
        selector.handle_input("\x1b");
        assert_eq!(selector.drain(), vec![SettingsSelectorEvent::Cancel]);
    }

    /// The rows this port applies live deliver their events; the rest carry
    /// the raw value the orchestrator persists.
    #[test]
    fn the_persist_only_rows_deliver_their_events() {
        let events = queue();
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        // Rows 3-9 (0-based): skill-commands … terminal-progress.
        for _ in 0..3 {
            selector.handle_input("\x1b[B");
        }
        selector.handle_input("\r"); // skill-commands: true → false
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // show-hardware-cursor: false → true
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // editor-padding: 0 → 1
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // output-padding: 1 → 0
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // autocomplete-max-visible: 5 → 7
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // clear-on-shrink: false → true
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // terminal-progress: false → true
        assert_eq!(
            selector.drain(),
            vec![
                SettingsSelectorEvent::EnableSkillCommandsChange(false),
                SettingsSelectorEvent::ShowHardwareCursorChange(true),
                SettingsSelectorEvent::EditorPaddingXChange(1),
                SettingsSelectorEvent::OutputPadChange(0),
                SettingsSelectorEvent::AutocompleteMaxVisibleChange(7),
                SettingsSelectorEvent::ClearOnShrinkChange(true),
                SettingsSelectorEvent::ShowTerminalProgressChange(true),
            ]
        );
    }

    #[test]
    fn the_theme_submenu_previews_and_persists_through_done() {
        let events = queue();
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        // Navigate to the theme row (the last of thirty-two) and open its
        // submenu.
        for _ in 0..31 {
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
        let mut selector = SettingsSelectorComponent::new(&config(), events.clone());
        for _ in 0..31 {
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
        // Escape closes the automatic menu and restores the preview
        // (`settings-selector.ts:416`).
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        for _ in 0..31 {
            selector.handle_input("\x1b[B");
        }
        selector.handle_input("\r"); // open theme submenu
        selector.handle_input("\x1b[B");
        selector.handle_input("\r"); // automatic menu
        selector.handle_input("\x1b");
        assert!(selector.settings_list().selected_item().is_some());
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
        // Row 25 (0-based): model-thinking.
        for _ in 0..25 {
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
        // Row 21 (0-based): default-project-trust.
        for _ in 0..21 {
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
    /// and the theme (`settings-selector.ts:719-759`), and each delivers its
    /// event with the row's raw value.
    #[test]
    fn the_fullscreen_rows_deliver_their_events() {
        let events = queue();
        let mut selector = SettingsSelectorComponent::new(&config(), events);
        // Row 26 (0-based): tui-mode.
        for _ in 0..26 {
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
    /// current count deduplicated and sorted (`settings-selector.ts:747-758`).
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

    /// `parse_int` reads the controlled numeric row values.
    #[test]
    fn parse_int_reads_the_row_values() {
        assert_eq!(parse_int("60"), 60);
        assert_eq!(parse_int(" 3 "), 3);
        assert_eq!(parse_int(""), 0);
    }

    /// The HTTP idle timeout row's values are the choice labels, and the
    /// current value resolves through `formatHttpIdleTimeoutMs`.
    #[test]
    fn http_idle_timeout_labels_are_the_choices() {
        assert_eq!(
            HTTP_IDLE_TIMEOUT_CHOICES
                .iter()
                .map(|(label, _)| (*label).to_string())
                .collect::<Vec<_>>(),
            ["30 sec", "1 min", "2 min", "5 min", "disabled"]
        );
        assert_eq!(format_http_idle_timeout_ms(300_000), "5 min");
        assert_eq!(format_http_idle_timeout_ms(45_000), "45 sec");
    }

    /// The warnings submenu toggles the shared state and publishes the whole
    /// warnings object; escape closes through done.
    #[test]
    fn the_warnings_submenu_toggles_and_publishes_the_object() {
        let events = queue();
        let mut config = config();
        config.warnings.insert(
            "customKey".to_string(),
            Value::String("rides along".to_string()),
        );
        let state = Arc::new(Mutex::new(SelectorState {
            model_thinking_levels: BTreeMap::new(),
            warnings: config.warnings.clone(),
        }));
        let done = SubmenuDone::new();
        let mut submenu = WarningSettingsSubmenu::new(&state, events, done.clone());
        // The stored default applies: no anthropicExtraUsage → true.
        let lines = plain(&submenu, 80);
        assert!(lines.iter().any(|line| line.contains("true")), "{lines:?}");
        submenu.handle_key("\r"); // true → false
        let drained = std::mem::take(&mut *submenu.events.lock().expect("selector events"));
        assert_eq!(drained.len(), 1);
        let SettingsSelectorEvent::WarningsChange(warnings) = &drained[0] else {
            panic!("expected WarningsChange: {:?}", drained[0]);
        };
        assert_eq!(
            warnings.get("anthropicExtraUsage"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            warnings.get("customKey"),
            Some(&Value::String("rides along".to_string()))
        );
        // The shared state carries the change to the next open.
        let state = state.lock().expect("selector state");
        assert_eq!(
            state.warnings.get("anthropicExtraUsage"),
            Some(&Value::Bool(false))
        );
        drop(state);
        // Escape closes through done.
        submenu.handle_key("\x1b");
        let close = done.take_close().expect("close");
        assert_eq!(close.value, None);
    }

    /// The quiet-startup row cycles `true` → `header` → `false` and
    /// normalizes each into its variant (`settings-selector.ts:939-941`).
    #[test]
    fn quiet_startup_maps_the_three_values() {
        let events = queue();
        let mut config = config();
        config.quiet_startup = "header".to_string();
        let mut selector = SettingsSelectorComponent::new(&config, events);
        // Row 19 (0-based): quiet-startup.
        for _ in 0..19 {
            selector.handle_input("\x1b[B");
        }
        selector.handle_input("\r"); // header → false
        selector.handle_input("\r"); // false → true
        selector.handle_input("\r"); // true → header
        assert_eq!(
            selector.drain(),
            vec![
                SettingsSelectorEvent::QuietStartupChange(QuietStartup::Off),
                SettingsSelectorEvent::QuietStartupChange(QuietStartup::Quiet),
                SettingsSelectorEvent::QuietStartupChange(QuietStartup::Header),
            ]
        );
    }
}
