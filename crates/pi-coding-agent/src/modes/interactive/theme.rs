//! The interactive mode's global theme, pi's app-level keybindings, and the
//! key-hint formatting helpers.
//!
//! Ground truth: `modes/interactive/theme/theme.ts` (the global instance and
//! the theme getters), `modes/interactive/theme/theme-tokens.ts` (token names
//! — the tokens themselves live in `pi_tui::theme`), `core/keybindings.ts`
//! (`APP_KEYBINDINGS`), and `modes/interactive/components/keybinding-hints.ts`.

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
/// reply).
fn terminal_colors() -> TerminalColors {
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
pub fn bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[22m")
}

pub fn italic(text: &str) -> String {
    format!("\x1b[3m{text}\x1b[23m")
}

pub fn underline(text: &str) -> String {
    format!("\x1b[4m{text}\x1b[24m")
}

pub fn strikethrough(text: &str) -> String {
    format!("\x1b[9m{text}\x1b[29m")
}

pub fn inverse(text: &str) -> String {
    format!("\x1b[7m{text}\x1b[27m")
}

/// A token styler for hosts that need `theme.fg(...)` as a closure.
pub fn fg_styler(token: pi_tui::theme::ThemeColor) -> Styler {
    theme().fg_styler(token)
}

// ============================================================
// App keybindings (`core/keybindings.ts` APP_KEYBINDINGS).
// ============================================================

pub mod app_keys {
    /// The app-level actions this port binds (`APP_KEYBINDINGS` subset — the
    /// actions whose handlers exist in the interactive mode; the dialog-only
    /// actions join with their dialogs).
    pub const INTERRUPT: &str = "app.interrupt";
    pub const CLEAR: &str = "app.clear";
    pub const EXIT: &str = "app.exit";
    pub const SUSPEND: &str = "app.suspend";
    pub const THINKING_CYCLE: &str = "app.thinking.cycle";
    pub const MODEL_CYCLE_FORWARD: &str = "app.model.cycleForward";
    pub const MODEL_CYCLE_BACKWARD: &str = "app.model.cycleBackward";
    pub const TOOLS_EXPAND: &str = "app.tools.expand";
    pub const THINKING_TOGGLE: &str = "app.thinking.toggle";
    pub const MESSAGE_FOLLOW_UP: &str = "app.message.followUp";
    pub const MESSAGE_DEQUEUE: &str = "app.message.dequeue";
    /// Dialog-only actions (`keybindings.ts:86-104`), bound inside their
    /// selectors.
    pub const SESSION_TOGGLE_NAMED_FILTER: &str = "app.session.toggleNamedFilter";
    pub const SESSION_TOGGLE_PATH: &str = "app.session.togglePath";
    pub const SESSION_TOGGLE_SORT: &str = "app.session.toggleSort";
    pub const SESSION_RENAME: &str = "app.session.rename";
    pub const SESSION_DELETE: &str = "app.session.delete";
    pub const SESSION_DELETE_NONINVASIVE: &str = "app.session.deleteNoninvasive";

    /// pi's default keys, in `APP_KEYBINDINGS` order.
    pub const DEFAULTS: &[(&str, &[&str])] = &[
        (INTERRUPT, &["escape"]),
        (CLEAR, &["ctrl+c"]),
        (EXIT, &["ctrl+d"]),
        (SUSPEND, &["ctrl+z"]),
        (THINKING_CYCLE, &["shift+tab"]),
        (MODEL_CYCLE_FORWARD, &["ctrl+p"]),
        (MODEL_CYCLE_BACKWARD, &["ctrl+shift+p"]),
        (TOOLS_EXPAND, &["ctrl+o"]),
        (THINKING_TOGGLE, &["ctrl+t"]),
        (MESSAGE_FOLLOW_UP, &["alt+enter"]),
        (MESSAGE_DEQUEUE, &["alt+up"]),
        (SESSION_TOGGLE_NAMED_FILTER, &["ctrl+n"]),
        (SESSION_TOGGLE_PATH, &["ctrl+p"]),
        (SESSION_TOGGLE_SORT, &["ctrl+s"]),
        (SESSION_RENAME, &["ctrl+r"]),
        (SESSION_DELETE, &["ctrl+d"]),
        (SESSION_DELETE_NONINVASIVE, &["ctrl+backspace"]),
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
