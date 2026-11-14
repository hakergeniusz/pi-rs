//! The interactive mode's global theme, pi's app-level keybindings, and the
//! key-hint formatting helpers.
//!
//! Ground truth: `modes/interactive/theme/theme.ts` (the global instance and
//! the theme getters), `modes/interactive/theme/theme-tokens.ts` (token names
//! — the tokens themselves live in `pi_tui::theme`), `core/keybindings.ts`
//! (`APP_KEYBINDINGS`), and `modes/interactive/components/keybinding-hints.ts`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use pi_tui::color::{TerminalColorMode, TerminalColors};
use pi_tui::theme::{
    Styler, Theme, ThemeAppearance, ThemePaths, ThemeRegistry, detect_terminal_theme, load_theme,
};

use crate::config;

/// The global theme instance, pi's `theme` proxy (`theme.ts:648-659`).
static THEME: RwLock<Option<Arc<Theme>>> = RwLock::new(None);

/// The terminal colors reported so far (`theme.ts:133`). The OSC color query
/// wiring lands with the theme controller (P6 part 2); until then this stays
/// empty and the system theme renders from the detected appearance.
static TERMINAL_COLORS: OnceLock<TerminalColors> = OnceLock::new();

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

/// `getTerminalTheme()` (`theme.ts:639-641`): dark or light from everything
/// reported so far. Without an OSC report this falls back to COLORFGBG, then
/// dark, exactly like upstream.
pub fn terminal_theme() -> &'static str {
    // `detect_terminal_theme` consumes a borrowed string, so bind the env read.
    let colorfgbg = colorfgbg();
    match detect_terminal_theme(&terminal_colors(), None, colorfgbg.as_deref()) {
        ThemeAppearance::Light => "light",
        ThemeAppearance::Dark => "dark",
    }
}

/// The directory custom themes load from: `<agentDir>/themes`
/// (`getCustomThemesDir`, `config.ts:572-574`).
fn custom_themes_dir() -> PathBuf {
    config::get_agent_dir().join("themes")
}

/// The terminal colors reported so far (empty until the OSC query is wired).
fn terminal_colors() -> TerminalColors {
    TERMINAL_COLORS.get().cloned().unwrap_or_default()
}

fn load_theme_by_name(name: &str) -> Result<Theme, String> {
    let terminal = terminal_colors();
    load_theme(
        &ThemePaths::new("/nonexistent", custom_themes_dir()),
        &ThemeRegistry::new(),
        name,
        terminal_color_mode(),
        &terminal,
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
