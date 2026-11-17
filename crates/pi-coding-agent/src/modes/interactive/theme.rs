//! The interactive mode's global theme, pi's app-level keybindings, and the
//! key-hint formatting helpers.
//!
//! Ground truth: `modes/interactive/theme/theme.ts` (the global instance and
//! the theme getters, plus the "HTML Export Helpers" section `:834-897` that
//! `crate::session::export_html` consumes), `modes/interactive/theme/theme-tokens.ts`
//! (token names — the tokens themselves live in `pi_tui::theme`),
//! `core/keybindings.ts` (`APP_KEYBINDINGS`), and
//! `modes/interactive/components/keybinding-hints.ts`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use pi_tui::color::{TerminalColorMode, TerminalColors};
use pi_tui::oklab::Rgb;
use pi_tui::theme::{
    Styler, SystemThemeMode, Theme, ThemeAppearance, ThemePaths, ThemeRegistry,
    detect_terminal_theme, load_theme_with_system,
};

use crate::config;

/// The global theme instance, pi's `theme` proxy (`theme.ts:648-659`).
static THEME: RwLock<Option<Arc<Theme>>> = RwLock::new(None);

/// The terminal's reported colors (`theme.ts:131-136`): replaced, never
/// mutated, on every OSC reply, and `None` until the first report.
static TERMINAL_COLORS: RwLock<Option<TerminalColors>> = RwLock::new(None);
/// While a color query is in flight, the system theme renders in grayscale
/// (`theme.ts:137-139`).
static TERMINAL_COLORS_PENDING: AtomicBool = AtomicBool::new(false);
/// The terminal's last light/dark report (mode 2031); only consulted before
/// it has reported a background (`theme.ts:140-143`).
static TERMINAL_COLOR_SCHEME: RwLock<Option<ThemeAppearance>> = RwLock::new(None);

/// `getTerminalColorMode()`: COLORTERM decides between 256-color and
/// truecolor (`packages/tui/src/colors.ts:91-148`).
fn terminal_color_mode() -> TerminalColorMode {
    match std::env::var("COLORTERM").as_deref() {
        Ok(value) if value.eq_ignore_ascii_case("truecolor") || value == "24bit" => {
            TerminalColorMode::TrueColor
        }
        _ => TerminalColorMode::Color256,
    }
}

/// The raw `COLORFGBG` value, when set (`detectColorFgBgTheme` input).
fn colorfgbg() -> Option<String> {
    std::env::var("COLORFGBG").ok()
}

/// The appearance [`terminal_theme`] reports, resolved by the loader's
/// detector from everything the terminal reported so far.
pub fn terminal_appearance() -> ThemeAppearance {
    // `detect_terminal_theme` consumes a borrowed string, so bind the env read.
    let colorfgbg = colorfgbg();
    detect_terminal_theme(&terminal_colors(), terminal_scheme(), colorfgbg.as_deref())
}

/// `getTerminalTheme()` (`theme.ts:639-641`): dark or light from everything
/// reported so far. Without an OSC report this falls back to the mode-2031
/// report, COLORFGBG, then dark, exactly like upstream.
pub fn terminal_theme() -> &'static str {
    match terminal_appearance() {
        ThemeAppearance::Light => "light",
        ThemeAppearance::Dark => "dark",
    }
}

/// `resolveThemeName` (`theme-controller.ts:168-171`): the setting resolved
/// against the live terminal appearance.
pub fn resolve_theme_setting(theme_setting: Option<&str>) -> Option<String> {
    pi_tui::theme::resolve_theme_setting(theme_setting, terminal_appearance())
}

/// The directory custom themes load from: `<agentDir>/themes`
/// (`getCustomThemesDir`, `config.ts:572-574`).
fn custom_themes_dir() -> PathBuf {
    config::get_agent_dir().join("themes")
}

/// The terminal colors reported so far (empty until the controller records a
/// reply). The 3D easter egg reads these for its fade colors.
pub(crate) fn terminal_colors() -> TerminalColors {
    TERMINAL_COLORS
        .read()
        .expect("terminal colors lock")
        .clone()
        .unwrap_or_default()
}

/// The terminal's light/dark report, when it made one.
fn terminal_scheme() -> Option<ThemeAppearance> {
    *TERMINAL_COLOR_SCHEME.read().expect("terminal scheme lock")
}

/// Whether a color query is still outstanding (grayscale system theme).
fn colors_pending() -> bool {
    TERMINAL_COLORS_PENDING.load(Ordering::SeqCst)
}

/// `sameTerminalColors` (`theme-controller.ts:56-64`): the derived equality
/// is the same field-wise comparison upstream writes by hand.
fn same_terminal_colors(a: &TerminalColors, b: &TerminalColors) -> bool {
    a == b
}

/// `setTerminalColors` (`theme.ts:143-147`) plus the controller's merge and
/// change check (`theme-controller.ts:187-203`): reported fields fall back to
/// the previous report, and the result records only when something changed.
/// Returns whether the report was new — the caller re-applies and re-renders.
pub fn set_terminal_colors(colors: TerminalColors) -> bool {
    let mut guard = TERMINAL_COLORS.write().expect("terminal colors lock");
    let previous = guard.clone().unwrap_or_default();
    let next = TerminalColors {
        foreground: colors.foreground.or(previous.foreground),
        background: colors.background.or(previous.background),
        palette: colors.palette.or_else(|| previous.palette.clone()),
    };
    TERMINAL_COLORS_PENDING.store(false, Ordering::SeqCst);
    if same_terminal_colors(&previous, &next) {
        return false;
    }
    *guard = Some(next);
    true
}

/// `setTerminalColorScheme` (`theme.ts:149-152`).
pub fn set_terminal_color_scheme(scheme: ThemeAppearance) {
    *TERMINAL_COLOR_SCHEME.write().expect("terminal scheme lock") = Some(scheme);
}

/// `markTerminalColorsPending` (`theme.ts:154-156`).
pub fn mark_terminal_colors_pending() {
    TERMINAL_COLORS_PENDING.store(true, Ordering::SeqCst);
}

/// The controller's `applyTerminalColors` seam: the TUI's query API reports
/// the `tui` module's `TerminalColors` (`RgbColor`, u8 channels) while the
/// loader consumes this module's shape (`Rgb`, f64). Upstream has a single
/// type; the port converts here, then merges like `setTerminalColors`.
pub fn record_reported_colors(reported: pi_tui::tui::TerminalColors) -> bool {
    let rgb = |color: pi_tui::terminal_colors::RgbColor| Rgb {
        r: f64::from(color.r),
        g: f64::from(color.g),
        b: f64::from(color.b),
    };
    set_terminal_colors(TerminalColors {
        foreground: reported.foreground.map(rgb),
        background: reported.background.map(rgb),
        palette: reported.palette.map(|palette| palette.map(rgb).to_vec()),
    })
}

// ============================================================
// Test seams: the terminal-color state is process-wide, so the tests that
// drive it (here and in the interactive-mode e2e) serialize on the lock and
// reset the globals between cases.
// ============================================================

#[cfg(test)]
pub(crate) static COLOR_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn reset_terminal_color_state() {
    *TERMINAL_COLORS.write().expect("terminal colors lock") = None;
    TERMINAL_COLORS_PENDING.store(false, Ordering::SeqCst);
    *TERMINAL_COLOR_SCHEME.write().expect("terminal scheme lock") = None;
}

#[cfg(test)]
pub(crate) fn terminal_scheme_for_tests() -> Option<ThemeAppearance> {
    terminal_scheme()
}

fn load_theme_by_name(name: &str) -> Result<Theme, String> {
    let terminal = terminal_colors();
    load_theme_with_system(
        &ThemePaths::new("/nonexistent", custom_themes_dir()),
        &ThemeRegistry::new(),
        name,
        terminal_color_mode(),
        &terminal,
        SystemThemeMode {
            colors_pending: colors_pending(),
            appearance_hint: terminal_scheme(),
        },
    )
}

/// `initTheme` (`theme.ts:687-701`): load `name` (default `system`); an
/// invalid name falls back to the system theme silently.
pub fn init_theme(name: Option<&str>) {
    let name = name.unwrap_or(pi_tui::theme::SYSTEM_THEME_NAME);
    let loaded =
        load_theme_by_name(name).or_else(|_| load_theme_by_name(pi_tui::theme::SYSTEM_THEME_NAME));
    if let Ok(theme) = loaded {
        *THEME.write().expect("theme lock") = Some(Arc::new(theme));
    }
}

/// The global theme. Initializes the system theme on first use, so tests and
/// hosts that never call [`init_theme`] still render with pi's default.
// The read guard must be dropped before `init_theme` takes the write lock —
// a `THEME.read().clone().unwrap_or_else(init)` chain keeps the guard
// temporary alive across the closure and deadlocks.
pub fn theme() -> Arc<Theme> {
    let cached = {
        let guard = THEME.read().expect("theme lock");
        guard.clone()
    };
    if let Some(theme) = cached {
        return theme;
    }
    init_theme(None);
    let guard = THEME.read().expect("theme lock");
    guard.clone().expect("theme initialized")
}

/// `setTheme` (`theme.ts:703-724`): switch the global theme; a failure keeps
/// the current one and reports the error.
pub fn set_theme(name: &str) -> Result<(), String> {
    let loaded = load_theme_by_name(name)?;
    *THEME.write().expect("theme lock") = Some(Arc::new(loaded));
    Ok(())
}

/// `getMarkdownTheme` (`theme.ts:1065-1083`): token stylers for the Markdown
/// component.
pub fn markdown_theme() -> pi_tui::components::markdown::MarkdownTheme {
    use pi_tui::components::markdown::MarkdownTheme;
    use pi_tui::theme::ThemeColor as C;
    let theme = theme();
    MarkdownTheme {
        heading: theme.fg_styler(C::MdHeading),
        link: theme.fg_styler(C::MdLink),
        link_url: theme.fg_styler(C::MdLinkUrl),
        code: theme.fg_styler(C::MdCode),
        code_block: theme.fg_styler(C::MdCodeBlock),
        code_block_border: theme.fg_styler(C::MdCodeBlockBorder),
        quote: theme.fg_styler(C::MdQuote),
        quote_border: theme.fg_styler(C::MdQuoteBorder),
        hr: theme.fg_styler(C::MdHr),
        list_bullet: theme.fg_styler(C::MdListBullet),
        bold: Arc::new(move |text: &str| bold(text)),
        italic: Arc::new(move |text: &str| italic(text)),
        strikethrough: Arc::new(move |text: &str| strikethrough(text)),
        underline: Arc::new(move |text: &str| underline(text)),
        highlight_code: None,
        code_block_indent: "  ".to_string(),
    }
}

/// `getSelectListTheme` (`theme.ts:1085-1093`).
pub fn select_list_theme() -> pi_tui::components::select_list::SelectListTheme {
    use pi_tui::theme::ThemeColor as C;
    let theme = theme();
    pi_tui::components::select_list::SelectListTheme {
        selected_prefix: theme.fg_styler(C::Accent),
        selected_text: theme.fg_styler(C::Accent),
        description: theme.fg_styler(C::Muted),
        scroll_info: theme.fg_styler(C::Muted),
        no_match: theme.fg_styler(C::Muted),
    }
}

/// `getSettingsListTheme` (`theme.ts:908-916`).
pub fn settings_list_theme() -> pi_tui::components::settings_list::SettingsListTheme {
    use pi_tui::theme::ThemeColor as C;
    let theme = theme();
    pi_tui::components::settings_list::SettingsListTheme {
        label: {
            let styler = theme.fg_styler(C::Accent);
            Arc::new(move |text: &str, selected: bool| {
                if selected {
                    styler(text)
                } else {
                    text.to_string()
                }
            })
        },
        value: {
            let accent = theme.fg_styler(C::Accent);
            let muted = theme.fg_styler(C::Muted);
            Arc::new(
                move |text: &str, selected: bool| {
                    if selected { accent(text) } else { muted(text) }
                },
            )
        },
        description: theme.fg_styler(C::Dim),
        cursor: theme.fg(C::Accent, "→ "),
        hint: theme.fg_styler(C::Dim),
    }
}

/// `getAvailableThemes` (`theme.ts`): the system theme plus every loadable
/// theme from the custom themes directory. The same paths
/// [`load_theme_by_name`] resolves against.
pub fn available_theme_names() -> Vec<String> {
    let paths = pi_tui::theme::ThemePaths::new("/nonexistent", custom_themes_dir());
    let registry = pi_tui::theme::ThemeRegistry::new();
    pi_tui::theme::available_themes(&paths, &registry)
        .into_iter()
        .map(|info| info.name)
        .collect()
}

/// `getEditorTheme` (`theme.ts:1095-1100`).
pub fn editor_theme() -> pi_tui::components::editor::EditorTheme {
    use pi_tui::theme::ThemeColor as C;
    let theme = theme();
    pi_tui::components::editor::EditorTheme {
        border_color: theme.fg_styler(C::BorderMuted),
        select_list: select_list_theme(),
    }
}

// chalk-equivalent attribute helpers (`theme.ts:314-331` — chalk.bold etc.).
/// Wrap `text` in the theme's bold sequence.
pub fn bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[22m")
}

/// Wrap `text` in the theme's italic sequence.
pub fn italic(text: &str) -> String {
    format!("\x1b[3m{text}\x1b[23m")
}

/// Wrap `text` in the theme's underline sequence.
pub fn underline(text: &str) -> String {
    format!("\x1b[4m{text}\x1b[24m")
}

/// Wrap `text` in the theme's strikethrough sequence.
pub fn strikethrough(text: &str) -> String {
    format!("\x1b[9m{text}\x1b[29m")
}

/// Wrap `text` in the theme's inverse sequence.
pub fn inverse(text: &str) -> String {
    format!("\x1b[7m{text}\x1b[27m")
}

/// A token styler for hosts that need `theme.fg(...)` as a closure.
pub fn fg_styler(token: pi_tui::theme::ThemeColor) -> Styler {
    theme().fg_styler(token)
}

// ============================================================
// HTML export helpers (`theme.ts:834-897`, the "HTML Export Helpers"
// section) — consumed by `crate::session::export_html`.
// ============================================================

/// The name a color-less request resolves against: the live theme's own
/// name, else the reserved system name (`theme.ts:835-837`'s
/// `themeName ?? currentThemeName ?? SYSTEM_THEME_NAME`).
fn export_theme_name(theme_name: Option<&str>) -> String {
    theme_name.map_or_else(
        || {
            theme()
                .name()
                .unwrap_or(pi_tui::theme::SYSTEM_THEME_NAME)
                .to_string()
        },
        str::to_string,
    )
}

/// `getResolvedThemeColors` (`theme.ts:834-839`): every theme token as a CSS
/// hex string, in the port's token declaration order (upstream iterates the
/// resolved colors record, whose order follows the theme document; the values
/// are the same either way). Fails when the theme cannot load, like
/// upstream's `loadTheme` throw.
pub fn get_resolved_theme_colors(
    theme_name: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let name = export_theme_name(theme_name);
    let loaded = load_theme_by_name(&name)?;
    let mut colors = Vec::with_capacity(
        pi_tui::theme::ThemeColor::ALL.len() + pi_tui::theme::ThemeBg::ALL.len(),
    );
    for token in pi_tui::theme::ThemeColor::ALL {
        let color = pi_tui::color::color_to_hex(loaded.fg_color(*token));
        colors.push((token.as_str().to_string(), color));
    }
    for token in pi_tui::theme::ThemeBg::ALL {
        let color = pi_tui::color::color_to_hex(loaded.bg_color(*token));
        colors.push((token.as_str().to_string(), color));
    }
    Ok(colors)
}

/// `getThemeExportColors`'s return shape (`theme.ts:850-855`): the resolved
/// `pageBg`/`cardBg`/`infoBg` strings, `None` where upstream is `undefined`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThemeExportColors {
    pub page_bg: Option<String>,
    pub card_bg: Option<String>,
    pub info_bg: Option<String>,
}

/// `getThemeExportColors` (`theme.ts:849-897`): the theme document's explicit
/// `export` colors, resolved through `vars` references. `okhsl(...)` values
/// convert to hex because CSS has no `okhsl()`; hex, `oklch(...)`, and empty
/// strings pass through (`undefined` for the latter). Unloadable themes and
/// the system theme carry no export colors.
pub fn get_theme_export_colors(theme_name: Option<&str>) -> ThemeExportColors {
    let name = export_theme_name(theme_name);
    if name == pi_tui::theme::SYSTEM_THEME_NAME {
        return ThemeExportColors::default();
    }
    let paths = pi_tui::theme::ThemePaths::new("/nonexistent", custom_themes_dir());
    let registry = pi_tui::theme::ThemeRegistry::new();
    let Ok(json) = pi_tui::theme::load_theme_json(&paths, &registry, &name) else {
        return ThemeExportColors::default();
    };
    // The export colors end up in CSS, which understands hex and oklch()
    // values directly but not okhsl() (`theme.ts:872-874`). An unparseable
    // okhsl passes through raw where upstream would feed `colorToHex` an
    // `undefined`.
    let resolve = |key: &str| -> Option<String> {
        let value = json.export.as_ref()?.get(key)?;
        let resolved = value.resolve(&json.vars).ok()?;
        match resolved {
            pi_tui::theme::ColorValue::Default => None,
            pi_tui::theme::ColorValue::Index(index) => Some(pi_tui::color::color_to_hex(
                pi_tui::color::indexed_color(u16::from(index)),
            )),
            pi_tui::theme::ColorValue::Text(text) => {
                // `if (resolved === "") return undefined` — a `vars` entry
                // referencing the empty string lands here as `Text("")`.
                if text.is_empty() {
                    return None;
                }
                let lowered = text.to_ascii_lowercase();
                if lowered.starts_with("okhsl(") {
                    // An unparseable okhsl passes through raw where upstream
                    // would feed `colorToHex` an `undefined`.
                    return Some(match pi_tui::color::parse_color(&text) {
                        Ok(color) => pi_tui::color::color_to_hex(color),
                        Err(_) => text.clone(),
                    });
                }
                Some(text)
            }
        }
    };
    ThemeExportColors {
        page_bg: resolve("pageBg"),
        card_bg: resolve("cardBg"),
        info_bg: resolve("infoBg"),
    }
}

// ============================================================
// App keybindings (`core/keybindings.ts` APP_KEYBINDINGS).
// ============================================================

pub mod app_keys {
    /// The app-level actions this port binds (`APP_KEYBINDINGS` subset — the
    /// actions whose handlers exist in the interactive mode; the dialog-only
    /// actions join with their dialogs, and the display-only ones are listed
    /// because `/hotkeys` shows every upstream action's binding).
    pub const INTERRUPT: &str = "app.interrupt";
    /// Clear the screen.
    pub const CLEAR: &str = "app.clear";
    /// Exit pi.
    pub const EXIT: &str = "app.exit";
    /// Suspend to the shell (SIGTSTP).
    pub const SUSPEND: &str = "app.suspend";
    /// Cycle the thinking level forward.
    pub const THINKING_CYCLE: &str = "app.thinking.cycle";
    /// Cycle to the next model (ctrl+p).
    pub const MODEL_CYCLE_FORWARD: &str = "app.model.cycleForward";
    /// Open the model selector (`keybindings.ts:49`).
    pub const MODEL_SELECT: &str = "app.model.select";
    /// Cycle to the previous model.
    pub const MODEL_CYCLE_BACKWARD: &str = "app.model.cycleBackward";
    /// Toggle tool-row expansion (ctrl+o).
    pub const TOOLS_EXPAND: &str = "app.tools.expand";
    /// Toggle the hide-thinking setting.
    pub const THINKING_TOGGLE: &str = "app.thinking.toggle";
    /// Open the external editor (`keybindings.ts:59-62`).
    pub const EDITOR_EXTERNAL: &str = "app.editor.external";
    /// Copy the selection or last assistant message (`keybindings.ts:63-66`).
    pub const MESSAGE_COPY: &str = "app.message.copy";
    /// Queue the editor text as a follow-up (alt+enter).
    pub const MESSAGE_FOLLOW_UP: &str = "app.message.followUp";
    /// Dequeue the last queued message (alt+up).
    pub const MESSAGE_DEQUEUE: &str = "app.message.dequeue";
    /// Paste files on macOS, images, or text from clipboard
    /// (`keybindings.ts:75-78`).
    pub const CLIPBOARD_PASTE_IMAGE: &str = "app.clipboard.pasteImage";
    /// Dialog-only actions (`keybindings.ts:86-104`), bound inside their
    /// selectors.
    pub const SESSION_TOGGLE_NAMED_FILTER: &str = "app.session.toggleNamedFilter";
    /// Toggle the session selector's path display.
    pub const SESSION_TOGGLE_PATH: &str = "app.session.togglePath";
    /// Toggle the session selector's sort.
    pub const SESSION_TOGGLE_SORT: &str = "app.session.toggleSort";
    /// Rename the selected session.
    pub const SESSION_RENAME: &str = "app.session.rename";
    /// Trash-delete the selected session.
    pub const SESSION_DELETE: &str = "app.session.delete";
    /// Remove the selected session without touching its file on disk.
    pub const SESSION_DELETE_NONINVASIVE: &str = "app.session.deleteNoninvasive";
    /// Open the session tree (`keybindings.ts:80`; unbound by default).
    pub const SESSION_TREE: &str = "app.session.tree";
    /// Fork the current session (`keybindings.ts:81`; unbound by default).
    pub const SESSION_FORK: &str = "app.session.fork";
    /// Fold a tree branch or move to the previous branch segment
    /// (`keybindings.ts:83-88`).
    pub const TREE_FOLD_OR_UP: &str = "app.tree.foldOrUp";
    /// Unfold a tree branch or move to the next branch segment
    /// (`keybindings.ts:87-90`).
    pub const TREE_UNFOLD_OR_DOWN: &str = "app.tree.unfoldOrDown";
    /// Edit the selected entry's label (`keybindings.ts:91-94`).
    pub const TREE_EDIT_LABEL: &str = "app.tree.editLabel";
    /// Toggle the tree's label timestamps (`keybindings.ts:95-98`).
    pub const TREE_TOGGLE_LABEL_TIMESTAMP: &str = "app.tree.toggleLabelTimestamp";
    /// Tree filter: the default view (`keybindings.ts:143-146`).
    pub const TREE_FILTER_DEFAULT: &str = "app.tree.filter.default";
    /// Tree filter: hide tool results (`keybindings.ts:147-150`).
    pub const TREE_FILTER_NO_TOOLS: &str = "app.tree.filter.noTools";
    /// Tree filter: user messages only (`keybindings.ts:151-154`).
    pub const TREE_FILTER_USER_ONLY: &str = "app.tree.filter.userOnly";
    /// Tree filter: labeled entries only (`keybindings.ts:155-158`).
    pub const TREE_FILTER_LABELED_ONLY: &str = "app.tree.filter.labeledOnly";
    /// Tree filter: show everything (`keybindings.ts:159-162`).
    pub const TREE_FILTER_ALL: &str = "app.tree.filter.all";
    /// Cycle the tree filter forward (`keybindings.ts:163-166`).
    pub const TREE_FILTER_CYCLE_FORWARD: &str = "app.tree.filter.cycleForward";
    /// Cycle the tree filter backward (`keybindings.ts:167-170`).
    pub const TREE_FILTER_CYCLE_BACKWARD: &str = "app.tree.filter.cycleBackward";

    /// pi's default keys, in `APP_KEYBINDINGS` order.
    pub const DEFAULTS: &[(&str, &[&str])] = &[
        (INTERRUPT, &["escape"]),
        (CLEAR, &["ctrl+c"]),
        (EXIT, &["ctrl+d"]),
        (SUSPEND, &["ctrl+z"]),
        (THINKING_CYCLE, &["shift+tab"]),
        (MODEL_CYCLE_FORWARD, &["ctrl+p"]),
        (MODEL_SELECT, &["ctrl+l"]),
        (TOOLS_EXPAND, &["ctrl+o"]),
        (THINKING_TOGGLE, &["ctrl+t"]),
        (EDITOR_EXTERNAL, &["ctrl+g"]),
        (MODEL_CYCLE_BACKWARD, &["ctrl+shift+p"]),
        (MESSAGE_COPY, &["ctrl+x"]),
        (MESSAGE_FOLLOW_UP, &["alt+enter"]),
        (MESSAGE_DEQUEUE, &["alt+up"]),
        (CLIPBOARD_PASTE_IMAGE, &["ctrl+v"]),
        (SESSION_TOGGLE_NAMED_FILTER, &["ctrl+n"]),
        (SESSION_TOGGLE_PATH, &["ctrl+p"]),
        (SESSION_TOGGLE_SORT, &["ctrl+s"]),
        (SESSION_RENAME, &["ctrl+r"]),
        (SESSION_DELETE, &["ctrl+d"]),
        (SESSION_DELETE_NONINVASIVE, &["ctrl+backspace"]),
        // `app.session.tree`/`app.session.fork` are unbound by default
        // (`keybindings.ts:80-82`); the port reaches them via /tree and /fork.
        (SESSION_TREE, &[]),
        (SESSION_FORK, &[]),
        // The darwin variants swap to alt+first (`keybindings.ts:83-98`); the
        // port is POSIX-only and ships the non-darwin defaults.
        (TREE_FOLD_OR_UP, &["ctrl+left", "alt+left"]),
        (TREE_UNFOLD_OR_DOWN, &["ctrl+right", "alt+right"]),
        (TREE_EDIT_LABEL, &["shift+l"]),
        (TREE_TOGGLE_LABEL_TIMESTAMP, &["shift+t"]),
        (TREE_FILTER_DEFAULT, &["ctrl+d"]),
        (TREE_FILTER_NO_TOOLS, &["ctrl+t"]),
        (TREE_FILTER_USER_ONLY, &["ctrl+u"]),
        (TREE_FILTER_LABELED_ONLY, &["ctrl+l"]),
        (TREE_FILTER_ALL, &["ctrl+a"]),
        (TREE_FILTER_CYCLE_FORWARD, &["ctrl+o"]),
        (TREE_FILTER_CYCLE_BACKWARD, &["shift+ctrl+o"]),
    ];

    /// The keys bound to `action` (`keybindings.ts:60-70` getKeys).
    pub fn keys_for(action: &str) -> &'static [&'static str] {
        DEFAULTS
            .iter()
            .find(|(id, _)| *id == action)
            .map_or(&[], |(_, keys)| *keys)
    }

    /// Whether raw input matches the action's first bound key.
    pub fn matches(data: &str, action: &str) -> bool {
        keys_for(action)
            .iter()
            .any(|key| pi_tui::keys::matches_key(data, key))
    }
}

/// `formatKeyText` (`keybinding-hints.ts:17-27`): `a/b` alternatives and
/// `mod+key` chains pass through; the port has no darwin renaming to do.
pub fn format_key_text(key: &str) -> String {
    key.to_string()
}

/// `keyText` (`keybinding-hints.ts:34-36`): the first bound key of `action`,
/// or "" when unbound.
pub fn key_text(action: &str) -> String {
    app_keys::keys_for(action)
        .first()
        .map_or_else(String::new, |key| (*key).to_string())
}

/// `keyHint` (`keybinding-hints.ts:42-44`): dim key + muted description.
pub fn key_hint(action: &str, description: &str) -> String {
    use pi_tui::theme::ThemeColor as C;
    let theme = theme();
    format!(
        "{} {}",
        theme.fg(C::Dim, &key_text(action)),
        theme.fg(C::Muted, description)
    )
}

/// `keyDisplayText` (`keybinding-hints.ts:38-40`): every bound key of
/// `action` joined `a/b`, each `mod+key` part capitalized. Editor actions
/// resolve through [`pi_tui::keybindings::keys_for`], app actions through
/// [`app_keys::keys_for`].
pub fn key_display_text(action: &str) -> String {
    let keys = if action.starts_with("app.") {
        app_keys::keys_for(action)
    } else {
        pi_tui::keybindings::keys_for(action)
    };
    keys.iter()
        .map(|key| {
            key.split('+')
                .map(|part| match part.chars().next() {
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &part[first.len_utf8()..]
                    }
                    None => String::new(),
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `rawKeyHint` (`keybinding-hints.ts:46-48`).
pub fn raw_key_hint(key: &str, description: &str) -> String {
    use pi_tui::theme::ThemeColor as C;
    let theme = theme();
    format!(
        "{} {}",
        theme.fg(C::Dim, &format_key_text(key)),
        theme.fg(C::Muted, description)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_colors_merge_change_and_convert() {
        let _guard = COLOR_STATE_LOCK.lock().expect("color state lock");
        reset_terminal_color_state();
        mark_terminal_colors_pending();

        // First report: recorded, pending cleared, the TUI's u8 channels
        // converted to the loader's f64 shape.
        assert!(record_reported_colors(pi_tui::tui::TerminalColors {
            foreground: Some(pi_tui::terminal_colors::RgbColor {
                r: 255,
                g: 255,
                b: 255
            }),
            background: Some(pi_tui::terminal_colors::RgbColor { r: 0, g: 0, b: 0 }),
            palette: None,
        }));
        assert!(!colors_pending(), "a report clears the pending flag");
        assert_eq!(terminal_theme(), "dark");
        assert_eq!(
            terminal_colors().foreground.map(|color| color.r),
            Some(255.0)
        );

        // The same colors again: the merge fills the gaps and finds no change.
        assert!(!record_reported_colors(pi_tui::tui::TerminalColors {
            foreground: None,
            background: Some(pi_tui::terminal_colors::RgbColor { r: 0, g: 0, b: 0 }),
            palette: None,
        }));

        // A palette-only report changes the record and keeps fg/bg.
        assert!(set_terminal_colors(TerminalColors {
            foreground: None,
            background: None,
            palette: Some(vec![Rgb {
                r: 1.0,
                g: 2.0,
                b: 3.0
            }]),
        }));
        let stored = terminal_colors();
        assert_eq!(
            stored.palette,
            Some(vec![Rgb {
                r: 1.0,
                g: 2.0,
                b: 3.0
            }])
        );
        assert!(stored.foreground.is_some() && stored.background.is_some());

        // A changed background re-records and re-themes.
        assert!(record_reported_colors(pi_tui::tui::TerminalColors {
            foreground: None,
            background: Some(pi_tui::terminal_colors::RgbColor {
                r: 255,
                g: 255,
                b: 255
            }),
            palette: None,
        }));
        assert_eq!(terminal_theme(), "light");

        reset_terminal_color_state();
    }

    #[test]
    fn terminal_theme_schemes_sit_below_a_background() {
        let _guard = COLOR_STATE_LOCK.lock().expect("color state lock");
        reset_terminal_color_state();

        // Without a background report, the mode-2031 report decides.
        set_terminal_color_scheme(ThemeAppearance::Light);
        assert_eq!(terminal_theme(), "light");
        // A background report outranks the scheme report.
        assert!(set_terminal_colors(TerminalColors {
            foreground: None,
            background: Some(Rgb {
                r: 0.0,
                g: 0.0,
                b: 0.0
            }),
            palette: None,
        }));
        assert_eq!(terminal_theme(), "dark");

        reset_terminal_color_state();
    }

    #[test]
    fn app_keybinding_defaults_match_pi() {
        assert!(app_keys::matches("\x1b", app_keys::INTERRUPT));
        assert!(app_keys::matches("\x03", app_keys::CLEAR));
        assert!(app_keys::matches("\x04", app_keys::EXIT));
        assert!(app_keys::matches("\x0f", app_keys::TOOLS_EXPAND), "ctrl+o");
        assert!(
            app_keys::matches("\x14", app_keys::THINKING_TOGGLE),
            "ctrl+t"
        );
        assert!(
            app_keys::matches("\x10", app_keys::MODEL_CYCLE_FORWARD),
            "ctrl+p"
        );
        assert_eq!(app_keys::keys_for(app_keys::INTERRUPT), &["escape"]);
        assert_eq!(key_text(app_keys::INTERRUPT), "escape");
        assert_eq!(key_text("app.nonexistent"), "");
    }

    #[test]
    fn colorfgbg_classification_matches_vim() {
        // The boundaries documented upstream (`theme.ts:615-621`).
        let classify = |background: &str| -> Option<&'static str> {
            let index: u32 = background.parse().ok()?;
            if index > 15 {
                return None;
            }
            Some(if index <= 6 || index == 8 {
                "dark"
            } else {
                "light"
            })
        };
        assert_eq!(classify("0"), Some("dark"));
        assert_eq!(classify("6"), Some("dark"));
        assert_eq!(classify("8"), Some("dark"), "bright black (Solarized Dark)");
        assert_eq!(classify("7"), Some("light"));
        assert_eq!(classify("15"), Some("light"));
        assert_eq!(classify("16"), None);
        assert_eq!(classify("default"), None);
    }

    #[test]
    fn init_theme_falls_back_to_system() {
        init_theme(Some("definitely-not-a-theme"));
        // The fallback keeps rendering possible.
        let theme = theme();
        assert!(
            theme
                .fg(pi_tui::theme::ThemeColor::Accent, "x")
                .contains('x')
        );
    }

    #[test]
    fn theme_lazy_init_from_cold_does_not_deadlock() {
        // Regression: the read guard used to live across `init_theme`'s write
        // lock when the lazy path ran first. Force the cold path here.
        *THEME.write().expect("theme lock") = None;
        let theme = theme();
        assert!(
            theme
                .fg(pi_tui::theme::ThemeColor::Accent, "x")
                .contains('x')
        );
    }
}
