//! Theme tokens, theme JSON validation, theme loading and the theme file watcher.
//!
//! Ground truth: `packages/coding-agent/src/modes/interactive/theme/theme.ts`
//! (the `ThemeColor` / `ThemeBg` unions at `theme.ts:57-116`, `resolveVarRefs`
//! at `theme.ts:139-157`, `withThemeColorFallbacks` at `theme.ts:159-171`,
//! `loadTheme` / `loadThemeJson` at `theme.ts:625-638` and `theme.ts:551-574`,
//! `parseAutoThemeSetting` at `theme.ts:655-685`, the `COLORFGBG` and terminal
//! detection at `theme.ts:696-716`, and the file watcher at `theme.ts:811-897`),
//! `theme-json.ts` (the schema at `theme-json.ts:13-98` and the error assembly at
//! `theme-json.ts:95-142`), `system-theme.ts:389-431` (WCAG luminance, contrast
//! and `terminalAppearance`), `system-theme.ts:598-611` (`createSystemTheme`,
//! wired in [`generate_system_theme`]) and `packages/tui/src/colors.ts:91-148`
//! (the colour value grammar). The generation itself lives in
//! [`crate::system_theme`].
//!
//! The built-in themes ship as `crates/pi-tui/themes/{dark,light}.json`, the
//! verbatim contents of pi's `theme/dark.json` and `theme/light.json`, and are
//! embedded with `include_str!` so no filesystem lookup is needed for them.
//!
//! Nothing here owns a thread, a timer or global mutable state. Paths and the
//! terminal's reported colours are passed in explicitly, and the host drives
//! the watcher by calling [`ThemeWatcher::should_reload`] with its own
//! millisecond clock.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde::Deserialize;
use serde_json::Value;

use crate::color::{
    Color, ColorMixSpace, TerminalColorMode, TerminalColors, TextAttributes, background_ansi,
    foreground_ansi, indexed_color, mix_colors, rgb_color, style_text_with_ansi,
};
use crate::components::markdown::MarkdownTheme;
use crate::components::markdown::markdown_extras::{
    SyntaxKind, highlight_segments, is_supported_language,
};
use crate::oklab::{Rgb, rgb_to_oklab};
use crate::system_theme::{SystemThemeColors, SystemThemeInput, generate_system_theme_colors};

/// A text styler: what the components in [`crate::components`] consume.
pub type Styler = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// The reserved name of the generated system theme (`system-theme.ts:35`).
pub const SYSTEM_THEME_NAME: &str = "system";

/// The two inputs pi's `createSystemTheme` adds to the terminal's reported
/// colors (`theme.ts:598-611`): whether a colour report is still outstanding
/// (the first frame renders in grayscale, `theme-controller.ts:88`), and the
/// terminal's appearance when it reported no background of its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemThemeMode {
    /// pi's `terminalColorsPending` (`theme.ts:190`, `theme.ts:201`).
    pub colors_pending: bool,
    /// pi's `appearanceHint` (`theme.ts:604`), used only when no background
    /// was reported.
    pub appearance_hint: Option<ThemeAppearance>,
}

impl SystemThemeMode {
    /// A terminal that has answered (or answered nothing at all): full
    /// saturation, no hint needed.
    pub fn reported() -> SystemThemeMode {
        SystemThemeMode::default()
    }
}

/// The built-in theme names (`theme.ts:443-456`).
pub const BUILTIN_THEME_NAMES: [&str; 2] = ["dark", "light"];

/// pi's built-in `theme/dark.json`, verbatim.
pub const DARK_THEME_JSON: &str = include_str!("../themes/dark.json");

/// pi's built-in `theme/light.json`, verbatim.
pub const LIGHT_THEME_JSON: &str = include_str!("../themes/light.json");

/// Debounce for theme file changes (`theme.ts:835-841`).
pub const THEME_WATCH_DEBOUNCE_MS: u64 = 100;

/// `system-theme.ts:429`.
pub const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

macro_rules! theme_tokens {
    ($doc:literal, $enum_name:ident, $all:ident, [$(($variant:ident, $name:literal)),+ $(,)?]) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $enum_name {
            $(
                #[doc = concat!("`", $name, "`")]
                $variant,
            )+
        }

        impl $enum_name {
            /// Every token, in pi's declaration order.
            pub const $all: &'static [$enum_name] = &[$($enum_name::$variant),+];

            /// The token's name as it appears in theme JSON.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($enum_name::$variant => $name),+
                }
            }

            /// The token with this JSON name, or `None`.
            pub fn from_name(name: &str) -> Option<$enum_name> {
                match name {
                    $($name => Some($enum_name::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

theme_tokens!(
    "A foreground token. A token is only accepted in a foreground slot because\n\
     `\"\"` means the terminal's default *foreground* there (`theme.ts:118-124`).",
    ThemeColor,
    ALL,
    [
        (Accent, "accent"),
        (Border, "border"),
        (BorderAccent, "borderAccent"),
        (BorderMuted, "borderMuted"),
        (Success, "success"),
        (Error, "error"),
        (Warning, "warning"),
        (Muted, "muted"),
        (Dim, "dim"),
        (Text, "text"),
        (ThinkingText, "thinkingText"),
        (ScrollbarTrack, "scrollbarTrack"),
        (ScrollbarThumb, "scrollbarThumb"),
        (SearchMatchText, "searchMatchText"),
        (UserMessageText, "userMessageText"),
        (CustomMessageText, "customMessageText"),
        (CustomMessageLabel, "customMessageLabel"),
        (ToolTitle, "toolTitle"),
        (ToolOutput, "toolOutput"),
        (MdHeading, "mdHeading"),
        (MdLink, "mdLink"),
        (MdLinkUrl, "mdLinkUrl"),
        (MdCode, "mdCode"),
        (MdCodeBlock, "mdCodeBlock"),
        (MdCodeBlockBorder, "mdCodeBlockBorder"),
        (MdQuote, "mdQuote"),
        (MdQuoteBorder, "mdQuoteBorder"),
        (MdHr, "mdHr"),
        (MdListBullet, "mdListBullet"),
        (ToolDiffAdded, "toolDiffAdded"),
        (ToolDiffRemoved, "toolDiffRemoved"),
        (ToolDiffContext, "toolDiffContext"),
        (SyntaxComment, "syntaxComment"),
        (SyntaxKeyword, "syntaxKeyword"),
        (SyntaxFunction, "syntaxFunction"),
        (SyntaxVariable, "syntaxVariable"),
        (SyntaxString, "syntaxString"),
        (SyntaxNumber, "syntaxNumber"),
        (SyntaxType, "syntaxType"),
        (SyntaxOperator, "syntaxOperator"),
        (SyntaxPunctuation, "syntaxPunctuation"),
        (ThinkingOff, "thinkingOff"),
        (ThinkingMinimal, "thinkingMinimal"),
        (ThinkingLow, "thinkingLow"),
        (ThinkingMedium, "thinkingMedium"),
        (ThinkingHigh, "thinkingHigh"),
        (ThinkingXhigh, "thinkingXhigh"),
        (ThinkingMax, "thinkingMax"),
        (BashMode, "bashMode"),
    ]
);

theme_tokens!(
    "A background token (`theme.ts:102-116`, `theme.ts:574-586`).",
    ThemeBg,
    ALL,
    [
        (SelectedBg, "selectedBg"),
        (SearchMatchBg, "searchMatchBg"),
        (UserMessageBg, "userMessageBg"),
        (CustomMessageBg, "customMessageBg"),
        (ToolPendingBg, "toolPendingBg"),
        (ToolSuccessBg, "toolSuccessBg"),
        (ToolErrorBg, "toolErrorBg"),
    ]
);

impl ThemeColor {
    /// Tokens the schema marks optional (`theme-json.ts:41-46`, `theme.ts:126`).
    pub fn is_optional(self) -> bool {
        matches!(
            self,
            ThemeColor::ScrollbarTrack
                | ThemeColor::ScrollbarThumb
                | ThemeColor::SearchMatchText
                | ThemeColor::ThinkingMax
        )
    }
}

impl ThemeBg {
    /// `searchMatchBg` is the only optional background token (`theme-json.ts:45`).
    pub fn is_optional(self) -> bool {
        matches!(self, ThemeBg::SearchMatchBg)
    }
}

/// The `colors` keys every theme must define: all tokens minus the optional ones
/// (`theme-json.ts:29-91`).
pub fn required_color_tokens() -> Vec<&'static str> {
    ThemeColor::ALL
        .iter()
        .filter(|token| !token.is_optional())
        .map(|token| token.as_str())
        .chain(
            ThemeBg::ALL
                .iter()
                .filter(|t| !t.is_optional())
                .map(|t| t.as_str()),
        )
        .collect()
}

/// The background a theme is designed for (`theme.ts:185-187`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeAppearance {
    Dark,
    Light,
}

impl ThemeAppearance {
    /// The name as it appears in theme JSON and theme settings: `dark` or `light`.
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeAppearance::Dark => "dark",
            ThemeAppearance::Light => "light",
        }
    }

    /// `theme.ts:463-464`. Parsing an appearance name; not a `FromStr`
    /// impl, because pi's theme setting accepts the `light/dark` auto form too.
    #[allow(clippy::should_implement_trait)]
    pub fn parse_name(value: &str) -> Option<ThemeAppearance> {
        match value {
            "dark" => Some(ThemeAppearance::Dark),
            "light" => Some(ThemeAppearance::Light),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for ThemeAppearance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        ThemeAppearance::parse_name(&value)
            .ok_or_else(|| serde::de::Error::custom("expected \"dark\" or \"light\""))
    }
}

// ============================================================================
// Colour values
// ============================================================================

/// One colour value: `"#rrggbb"`, `"oklch(...)"`, `"okhsl(...)"`, a `vars`
/// reference, `""` for the terminal default, or a 0-255 palette index
/// (`theme-json.ts:13-16`).
#[derive(Debug, Clone, PartialEq)]
pub enum ColorValue {
    /// `""`: the terminal's default colour for whichever slot the token sits in.
    Default,
    /// An integer 0-255: a palette index.
    Index(u8),
    /// Anything else, including a `vars` reference.
    Text(String),
}

impl ColorValue {
    /// Resolve `vars` references (`theme.ts:139-148`).
    pub fn resolve(&self, vars: &BTreeMap<String, ColorValue>) -> Result<ColorValue, String> {
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut current = self.clone();
        loop {
            let ColorValue::Text(text) = &current else {
                return Ok(current);
            };
            if text.is_empty()
                || text.starts_with('#')
                || starts_with_ci(text, "oklch(")
                || starts_with_ci(text, "okhsl(")
            {
                return Ok(current);
            }
            if !visited.insert(text.clone()) {
                return Err(format!("Circular variable reference detected: {text}"));
            }
            let Some(next) = vars.get(text) else {
                return Err(format!("Variable reference not found: {text}"));
            };
            current = next.clone();
        }
    }

    /// The colour this value denotes, or `None` for [`ColorValue::Default`] and
    /// unparseable text (`colors.ts:121-148`).
    pub fn to_color(&self) -> Option<Color> {
        match self {
            ColorValue::Default => None,
            ColorValue::Index(index) => Some(indexed_color(u16::from(*index))),
            ColorValue::Text(text) => parse_color(text),
        }
    }
}

impl<'de> Deserialize<'de> for ColorValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(text) => Ok(if text.is_empty() {
                ColorValue::Default
            } else {
                ColorValue::Text(text)
            }),
            Value::Number(number) => match number.as_u64() {
                Some(index) if index <= 255 => Ok(ColorValue::Index(index as u8)),
                _ => Err(serde::de::Error::custom(
                    "Expected an integer between 0 and 255",
                )),
            },
            other => Err(serde::de::Error::custom(format!(
                "Expected a string or an integer between 0 and 255, got {}",
                json_type_name(&other)
            ))),
        }
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn starts_with_ci(value: &str, prefix: &str) -> bool {
    value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Parse a colour string: `#rgb`, `#rrggbb`, `oklch(l c h)` or
/// `okhsl(h s l)` - exactly what pi's `parseColor` accepts
/// (`colors.ts:121-148`), delegated to [`crate::color::parse_color`] so the
/// grammar, the range checks and the anchoring live in one place.
///
/// pi throws on a value it cannot parse, and this returns `None` instead:
/// [`ColorValue::to_color`] resolves an unparseable colour to the terminal
/// default, which is the same outcome a theme that named no colour has.
pub fn parse_color(value: &str) -> Option<Color> {
    crate::color::parse_color(value).ok()
}

fn rgb_to_color(rgb: Rgb) -> Color {
    rgb_color(rgb.r, rgb.g, rgb.b)
}

// ============================================================================
// Theme JSON documents
// ============================================================================

/// The HTML export colours (`theme-json.ts:93-98`), resolved to colours.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThemeExportColors {
    pub page_bg: Option<Color>,
    pub card_bg: Option<Color>,
    pub info_bg: Option<Color>,
}

/// A parsed theme document (`theme-json.ts:20-98`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThemeJson {
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    pub name: String,
    #[serde(default)]
    pub appearance: Option<ThemeAppearance>,
    #[serde(default)]
    pub vars: BTreeMap<String, ColorValue>,
    pub colors: BTreeMap<String, ColorValue>,
    #[serde(default)]
    pub export: Option<BTreeMap<String, ColorValue>>,
}

impl ThemeJson {
    /// The built-in `dark` / `light` document (`theme.ts:443-456`).
    pub fn builtin(name: &str) -> Result<ThemeJson, String> {
        let content = match name {
            "dark" => DARK_THEME_JSON,
            "light" => LIGHT_THEME_JSON,
            _ => return Err(format!("Theme not found: {name}")),
        };
        parse_theme_json_content(name, content)
    }

    /// The export colours, resolved (`theme.ts:900-947`).
    pub fn resolved_export_colors(&self) -> Result<ThemeExportColors, String> {
        let mut out = ThemeExportColors::default();
        let Some(export) = &self.export else {
            return Ok(out);
        };
        let resolve = |key: &str| -> Result<Option<Color>, String> {
            match export.get(key) {
                None => Ok(None),
                Some(value) => Ok(value.resolve(&self.vars)?.to_color()),
            }
        };
        out.page_bg = resolve("pageBg")?;
        out.card_bg = resolve("cardBg")?;
        out.info_bg = resolve("infoBg")?;
        Ok(out)
    }
}

// ============================================================================
// Validation (`theme-json.ts:88-148`)
// ============================================================================

fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// Parse JSON without validating: any object with a `colors` map is accepted
/// (`theme.ts:541-547`).
pub fn parse_theme_json_loose(label: &str, value: &Value) -> Result<ThemeJson, String> {
    if !matches!(value.get("colors"), Some(Value::Object(_))) {
        return Err(format!(
            "Invalid theme \"{label}\": expected an object with a \"colors\" map."
        ));
    }
    serde_json::from_value(value.clone())
        .map_err(|error| format!("Invalid theme \"{label}\": {error}"))
}

/// Parse and validate one theme document (`theme-json.ts:88-142`).
pub fn validate_theme_json(label: &str, value: &Value) -> Result<ThemeJson, String> {
    let Some(object) = value.as_object() else {
        return Err(format!(
            "Invalid theme \"{label}\": expected an object with a \"colors\" map."
        ));
    };

    let mut missing: BTreeSet<&str> = BTreeSet::new();
    let mut others: Vec<String> = Vec::new();

    match object.get("name") {
        None => others.push("  - /: Expected string".to_string()),
        Some(Value::String(_)) => {}
        Some(_) => others.push("  - /name: Expected string".to_string()),
    }
    if let Some(schema) = object.get("$schema")
        && !schema.is_string()
    {
        others.push("  - /$schema: Expected string".to_string());
    }
    if let Some(appearance) = object.get("appearance") {
        let valid = appearance
            .as_str()
            .and_then(ThemeAppearance::parse_name)
            .is_some();
        if !valid {
            others.push("  - /appearance: Expected \"dark\" or \"light\"".to_string());
        }
    }
    if let Some(vars) = object.get("vars") {
        check_color_map("/vars", vars, &mut others);
    }
    match object.get("colors") {
        None => others.push("  - /: Expected object".to_string()),
        Some(Value::Object(colors)) => {
            for token in required_color_tokens() {
                if !colors.contains_key(token) {
                    missing.insert(token);
                }
            }
            for (key, entry) in colors {
                check_color_value(&format!("/colors/{key}"), entry, &mut others);
            }
        }
        Some(_) => others.push("  - /colors: Expected object".to_string()),
    }
    if let Some(export) = object.get("export") {
        match export {
            Value::Object(entries) => {
                for (key, entry) in entries {
                    check_color_value(&format!("/export/{key}"), entry, &mut others);
                }
            }
            _ => others.push("  - /export: Expected object".to_string()),
        }
    }

    if !missing.is_empty() || !others.is_empty() {
        // The template is pi's verbatim (`theme-json.ts:107-131`).
        let mut message = format!("Invalid theme \"{label}\":\n");
        if !missing.is_empty() {
            message.push_str("\nMissing required color tokens:\n");
            for token in &missing {
                {
                    use std::fmt::Write as _;
                    let _ = writeln!(message, "  - {token}");
                }
            }
            message.push_str("\nPlease add these colors to your theme's \"colors\" object.");
            message.push_str(
                "\nSee the built-in themes (dark.json, light.json) for reference values.",
            );
        }
        if !others.is_empty() {
            message.push_str("\n\nOther errors:\n");
            message.push_str(&others.join("\n"));
        }
        return Err(message);
    }

    let theme: ThemeJson = serde_json::from_value(value.clone())
        .map_err(|error| format!("Invalid theme \"{label}\": {error}"))?;
    assert_theme_name_is_valid(&theme.name)?;
    Ok(theme)
}

/// `theme.ts:528-534`.
pub fn assert_theme_name_is_valid(name: &str) -> Result<(), String> {
    if name.contains('/') {
        return Err(format!(
            "Invalid theme name \"{name}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
        ));
    }
    Ok(())
}

fn check_color_map(path: &str, value: &Value, others: &mut Vec<String>) {
    match value {
        Value::Object(entries) => {
            for (key, entry) in entries {
                check_color_value(&format!("{path}/{key}"), entry, others);
            }
        }
        _ => others.push(format!("  - {path}: Expected object")),
    }
}

fn check_color_value(path: &str, value: &Value, others: &mut Vec<String>) {
    match value {
        Value::String(_) => {}
        Value::Number(number) => match number.as_u64() {
            Some(index) if index <= 255 => {}
            _ => others.push(format!("  - {path}: Expected an integer between 0 and 255")),
        },
        other => others.push(format!(
            "  - {path}: Expected a string or an integer between 0 and 255, got {}",
            json_type_name(other)
        )),
    }
}

/// Parse theme file content, then validate it (`theme.ts:549-558`).
pub fn parse_theme_json_content(label: &str, content: &str) -> Result<ThemeJson, String> {
    let value: Value = serde_json::from_str(strip_bom(content))
        .map_err(|error| format!("Failed to parse theme {label}: {error}"))?;
    validate_theme_json(label, &value)
}

// ============================================================================
// Theme
// ============================================================================

/// pi's `ThemeStyle` (`theme.ts:126-131`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ThemeStyle {
    pub fg: Option<ThemeColor>,
    pub bg: Option<ThemeBg>,
    pub attributes: TextAttributes,
}

/// A resolved theme (`theme.ts:246-380`). A document-backed theme (`dark`,
/// `light`, or a custom JSON file) and the generated `system` theme both land
/// here; the latter through [`Theme::from_colors`].
#[derive(Debug, Clone)]
pub struct Theme {
    name: Option<String>,
    source_path: Option<String>,
    own_appearance: Option<ThemeAppearance>,
    mode: TerminalColorMode,
    terminal: TerminalColors,
    fg: BTreeMap<ThemeColor, Color>,
    bg: BTreeMap<ThemeBg, Color>,
    default_fg: Vec<ThemeColor>,
    default_bg: Vec<ThemeBg>,
    dim_tokens: Vec<ThemeColor>,
    export: ThemeExportColors,
}

impl Theme {
    /// Build a theme from a validated document (`theme.ts:599-606`).
    // `default_fg`/`default_bg` mirror pi's paired fg/bg handling (`theme.ts`).
    #[allow(clippy::similar_names)]
    pub fn from_json(
        json: &ThemeJson,
        mode: TerminalColorMode,
        terminal: &TerminalColors,
        source_path: Option<PathBuf>,
    ) -> Result<Theme, String> {
        // Optional tokens fall back to the token pi names (`theme.ts:274-279`).
        let mut values: BTreeMap<String, ColorValue> = json.colors.clone();
        if !values.contains_key("scrollbarTrack") {
            values.insert(
                "scrollbarTrack".into(),
                json.colors
                    .get("muted")
                    .cloned()
                    .unwrap_or(ColorValue::Default),
            );
        }
        if !values.contains_key("scrollbarThumb") {
            values.insert(
                "scrollbarThumb".into(),
                json.colors
                    .get("text")
                    .cloned()
                    .unwrap_or(ColorValue::Default),
            );
        }
        if !values.contains_key("thinkingMax") {
            values.insert(
                "thinkingMax".into(),
                json.colors
                    .get("thinkingXhigh")
                    .cloned()
                    .unwrap_or(ColorValue::Default),
            );
        }
        if !values.contains_key("searchMatchBg") {
            values.insert(
                "searchMatchBg".into(),
                json.colors
                    .get("selectedBg")
                    .cloned()
                    .unwrap_or(ColorValue::Default),
            );
        }
        if !values.contains_key("searchMatchText") {
            values.insert(
                "searchMatchText".into(),
                json.colors
                    .get("text")
                    .cloned()
                    .unwrap_or(ColorValue::Default),
            );
        }

        let mut fg = BTreeMap::new();
        let mut bg = BTreeMap::new();
        let mut default_fg = Vec::new();
        let mut default_bg = Vec::new();
        for (key, raw) in &values {
            let value = raw.resolve(&json.vars)?;
            let Some(color) = value.to_color() else {
                continue;
            };
            match ThemeBg::from_name(key) {
                Some(token) => {
                    bg.insert(token, color);
                }
                None => {
                    if let Some(token) = ThemeColor::from_name(key) {
                        fg.insert(token, color);
                    }
                    // Unknown colour keys are passed through by pi
                    // (`theme.ts:588-601`); this port has nowhere to put them.
                }
            }
        }
        // `""` tokens keep the terminal default for their own slot
        // (`theme.ts:286-296`).
        for (key, raw) in &values {
            if !matches!(raw, ColorValue::Default) {
                continue;
            }
            let resolved = raw.resolve(&json.vars)?;
            if !matches!(resolved, ColorValue::Default) {
                continue;
            }
            match ThemeBg::from_name(key) {
                Some(token) => default_bg.push(token),
                None => {
                    if let Some(token) = ThemeColor::from_name(key) {
                        default_fg.push(token);
                    }
                }
            }
        }

        Ok(Theme {
            name: Some(json.name.clone()),
            source_path: source_path.map(|path| path.display().to_string()),
            own_appearance: json.appearance,
            mode,
            terminal: terminal.clone(),
            fg,
            bg,
            default_fg,
            default_bg,
            dim_tokens: Vec::new(),
            export: json.resolved_export_colors()?,
        })
    }

    /// Mark foreground tokens as rendered faint (`theme.ts:322-330`, `theme.ts:337-341`).
    #[must_use]
    pub fn with_dim_tokens(mut self, tokens: Vec<ThemeColor>) -> Self {
        self.dim_tokens = tokens;
        self
    }

    /// `new Theme(...)` (`theme.ts:246-305`): a theme built from colour values
    /// rather than from a document, which is how the generated system theme
    /// arrives. Optional tokens fall back to the token pi names
    /// (`theme.ts:265-270`).
    ///
    /// pi throws on a colour it cannot parse; here an unparseable value
    /// resolves to the terminal default, the same outcome
    /// [`Theme::from_json`] gives. The system theme's values are hex strings,
    /// palette indices or `""`, so nothing it produces takes that path.
    // `fg`/`bg`, `default_fg`/`default_bg`, `concrete_fg`/`concrete_bg` mirror
    // pi's paired fg/bg handling (`theme.ts`).
    #[allow(clippy::similar_names)]
    pub fn from_colors(
        foregrounds: &BTreeMap<ThemeColor, ColorValue>,
        backgrounds: &BTreeMap<ThemeBg, ColorValue>,
        mode: TerminalColorMode,
        terminal: &TerminalColors,
        name: Option<String>,
        appearance: Option<ThemeAppearance>,
        dim: Vec<ThemeColor>,
    ) -> Theme {
        let pick = |token: ThemeColor, fallback: ThemeColor| -> Option<ColorValue> {
            foregrounds
                .get(&token)
                .or_else(|| foregrounds.get(&fallback))
                .cloned()
        };
        let mut fg = BTreeMap::new();
        let mut default_fg = Vec::new();
        let mut concrete_fg = Vec::new();
        for token in ThemeColor::ALL {
            let value = match token {
                ThemeColor::ScrollbarTrack => pick(*token, ThemeColor::Muted),
                ThemeColor::ScrollbarThumb | ThemeColor::SearchMatchText => {
                    pick(*token, ThemeColor::Text)
                }
                ThemeColor::ThinkingMax => pick(*token, ThemeColor::ThinkingXhigh),
                _ => foregrounds.get(token).cloned(),
            };
            let Some(value) = value else {
                continue;
            };
            match value.to_color() {
                Some(color) => {
                    concrete_fg.push(color);
                    fg.insert(*token, color);
                }
                None => default_fg.push(*token),
            }
        }

        let mut bg = BTreeMap::new();
        let mut default_bg = Vec::new();
        let mut concrete_bg = Vec::new();
        for token in ThemeBg::ALL {
            let value = match token {
                ThemeBg::SearchMatchBg => backgrounds
                    .get(token)
                    .or_else(|| backgrounds.get(&ThemeBg::SelectedBg))
                    .cloned(),
                _ => backgrounds.get(token).cloned(),
            };
            let Some(value) = value else {
                continue;
            };
            match value.to_color() {
                Some(color) => {
                    concrete_bg.push(color);
                    bg.insert(*token, color);
                }
                None => default_bg.push(*token),
            }
        }

        let own_appearance = appearance.or_else(|| {
            detect_appearance_of(
                average_lightness(concrete_fg.into_iter()),
                average_lightness(concrete_bg.into_iter()),
            )
        });

        Theme {
            name,
            source_path: None,
            own_appearance,
            mode,
            terminal: terminal.clone(),
            fg,
            bg,
            default_fg,
            default_bg,
            dim_tokens: dim,
            // The system theme has no export block: `getThemeExportColors`
            // returns `{}` for it (`theme.ts:901-905`).
            export: ThemeExportColors::default(),
        }
    }

    /// The theme's name, or `None` when the builder gave none.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The path the theme document was read from, when the caller supplied one.
    pub fn source_path(&self) -> Option<&str> {
        self.source_path.as_deref()
    }

    /// The [`TerminalColorMode`] this theme's escape sequences are built for.
    pub fn color_mode(&self) -> TerminalColorMode {
        self.mode
    }

    /// The resolved `export` block, for the HTML theme export. The system
    /// theme carries none.
    pub fn export_colors(&self) -> &ThemeExportColors {
        &self.export
    }

    /// The declared appearance, else the one detected from the theme's own
    /// colours, else the terminal's (`theme.ts:302-306`, `theme.ts:200-215`).
    pub fn appearance(&self) -> ThemeAppearance {
        if let Some(appearance) = self.own_appearance {
            return appearance;
        }
        if let Some(appearance) = self.detect_appearance() {
            return appearance;
        }
        terminal_appearance(
            self.terminal.background.unwrap_or(WHITE_RGB),
            self.terminal.foreground,
        )
    }

    fn detect_appearance(&self) -> Option<ThemeAppearance> {
        let foreground = average_lightness(self.fg.values().copied());
        let background = average_lightness(self.bg.values().copied());
        detect_appearance_of(foreground, background)
    }

    fn default_foreground(&self) -> Color {
        match self.terminal.foreground {
            Some(rgb) => rgb_to_color(rgb),
            None => match self.appearance() {
                ThemeAppearance::Dark => rgb_to_color(Rgb {
                    r: 229.0,
                    g: 229.0,
                    b: 231.0,
                }),
                ThemeAppearance::Light => rgb_to_color(Rgb {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                }),
            },
        }
    }

    fn default_background(&self) -> Color {
        match self.terminal.background {
            Some(rgb) => rgb_to_color(rgb),
            None => match self.appearance() {
                ThemeAppearance::Dark => rgb_to_color(Rgb {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                }),
                ThemeAppearance::Light => rgb_to_color(Rgb {
                    r: 255.0,
                    g: 255.0,
                    b: 255.0,
                }),
            },
        }
    }

    /// The concrete colour of a foreground token (`theme.ts:318-333`).
    pub fn fg_color(&self, token: ThemeColor) -> Color {
        if self.dim_tokens.contains(&token)
            && let Some(color) = self.fg.get(&token)
            && let Ok(mixed) =
                mix_colors(*color, self.default_background(), 0.4, ColorMixSpace::Oklch)
        {
            return mixed;
        }
        self.fg
            .get(&token)
            .copied()
            .unwrap_or_else(|| self.default_foreground())
    }

    /// The concrete colour of a background token.
    pub fn bg_color(&self, token: ThemeBg) -> Color {
        self.bg
            .get(&token)
            .copied()
            .unwrap_or_else(|| self.default_background())
    }

    /// The opening escape sequence for a foreground token (`theme.ts:395-400`).
    pub fn fg_ansi(&self, token: ThemeColor) -> String {
        if self.default_fg.contains(&token) {
            return "\u{1b}[39m".to_string();
        }
        foreground_ansi(self.fg_color(token), self.mode)
    }

    /// The opening escape sequence for a background token (`theme.ts:402-404`).
    pub fn bg_ansi(&self, token: ThemeBg) -> String {
        if self.default_bg.contains(&token) {
            return "\u{1b}[49m".to_string();
        }
        background_ansi(self.bg_color(token), self.mode)
    }

    /// `theme.fg` (`theme.ts:357-361`). The opening escape is always written,
    /// including for a token left to the terminal's default colour.
    pub fn fg(&self, token: ThemeColor, text: &str) -> String {
        if self.dim_tokens.contains(&token) {
            return format!("{}\u{1b}[2m{text}\u{1b}[22;39m", self.fg_ansi(token));
        }
        format!("{}{text}\u{1b}[39m", self.fg_ansi(token))
    }

    /// `theme.bg` (`theme.ts:363-366`).
    pub fn bg(&self, token: ThemeBg, text: &str) -> String {
        format!("{}{text}\u{1b}[49m", self.bg_ansi(token))
    }

    /// `theme.style` (`theme.ts:343-355`). A faint token turns the SGR 2
    /// attribute on for whatever else the style asks for (`theme.ts:345`).
    pub fn style(&self, text: &str, style: ThemeStyle) -> String {
        let fg = style.fg.map(|token| self.fg_ansi(token));
        let bg = style.bg.map(|token| self.bg_ansi(token));
        let mut attributes = style.attributes;
        if style
            .fg
            .is_some_and(|token| self.dim_tokens.contains(&token))
        {
            attributes.dim = true;
        }
        style_text_with_ansi(text, fg.as_deref(), bg.as_deref(), attributes)
    }

    /// A [`Styler`] for a foreground token, what [`crate::components`] take.
    pub fn fg_styler(self: &Arc<Self>, token: ThemeColor) -> Styler {
        let theme = Arc::clone(self);
        Arc::new(move |text: &str| theme.fg(token, text))
    }

    /// A [`Styler`] for a background token.
    pub fn bg_styler(self: &Arc<Self>, token: ThemeBg) -> Styler {
        let theme = Arc::clone(self);
        Arc::new(move |text: &str| theme.bg(token, text))
    }

    /// The markdown component's theme hooks (`markdown.ts:207-234`).
    pub fn markdown_theme(self: &Arc<Self>) -> MarkdownTheme {
        let bold = attribute_styler(TextAttributes {
            bold: true,
            ..TextAttributes::default()
        });
        let italic = attribute_styler(TextAttributes {
            italic: true,
            ..TextAttributes::default()
        });
        let strikethrough = attribute_styler(TextAttributes {
            strikethrough: true,
            ..TextAttributes::default()
        });
        let underline = attribute_styler(TextAttributes {
            underline: true,
            ..TextAttributes::default()
        });
        // pi's `highlightCode` (`.../interactive/theme/theme.ts:935-954,
        // 1081`): token kinds map onto the `syntax*` theme colours. Unknown
        // languages fall back to the plain `mdCodeBlock` colour.
        let highlight_theme = Arc::clone(self);
        let highlight_code: Arc<crate::components::markdown::CodeHighlighter> =
            Arc::new(move |code: &str, lang: Option<&str>| {
                let fg = |token: ThemeColor, text: &str| highlight_theme.fg(token, text);
                if !is_supported_language(lang) {
                    return code
                        .split('\n')
                        .map(|line| fg(ThemeColor::MdCodeBlock, line))
                        .collect();
                }
                highlight_segments(code, lang)
                    .into_iter()
                    .map(|segments| {
                        segments
                            .into_iter()
                            .map(|(kind, text)| match kind {
                                SyntaxKind::Keyword => fg(ThemeColor::SyntaxKeyword, &text),
                                SyntaxKind::Type => fg(ThemeColor::SyntaxType, &text),
                                SyntaxKind::Number => fg(ThemeColor::SyntaxNumber, &text),
                                SyntaxKind::String => fg(ThemeColor::SyntaxString, &text),
                                SyntaxKind::Comment => fg(ThemeColor::SyntaxComment, &text),
                                SyntaxKind::Function => fg(ThemeColor::SyntaxFunction, &text),
                                SyntaxKind::Variable => fg(ThemeColor::SyntaxVariable, &text),
                                SyntaxKind::Operator => fg(ThemeColor::SyntaxOperator, &text),
                                SyntaxKind::Punctuation => fg(ThemeColor::SyntaxPunctuation, &text),
                                SyntaxKind::Plain => text,
                            })
                            .collect::<String>()
                    })
                    .collect()
            });
        MarkdownTheme {
            heading: self.fg_styler(ThemeColor::MdHeading),
            link: self.fg_styler(ThemeColor::MdLink),
            link_url: self.fg_styler(ThemeColor::MdLinkUrl),
            code: self.fg_styler(ThemeColor::MdCode),
            code_block: self.fg_styler(ThemeColor::MdCodeBlock),
            code_block_border: self.fg_styler(ThemeColor::MdCodeBlockBorder),
            quote: self.fg_styler(ThemeColor::MdQuote),
            quote_border: self.fg_styler(ThemeColor::MdQuoteBorder),
            hr: self.fg_styler(ThemeColor::MdHr),
            list_bullet: self.fg_styler(ThemeColor::MdListBullet),
            bold,
            italic,
            strikethrough,
            underline,
            highlight_code: Some(highlight_code),
            code_block_indent: "  ".to_string(),
        }
    }
}

fn attribute_styler(attributes: TextAttributes) -> Styler {
    Arc::new(move |text: &str| style_text_with_ansi(text, None, None, attributes))
}

const WHITE_RGB: Rgb = Rgb {
    r: 255.0,
    g: 255.0,
    b: 255.0,
};

/// `detectAppearance` (`theme.ts:216-223`), split out so both theme
/// constructors share it.
fn detect_appearance_of(
    foreground: Option<f64>,
    background: Option<f64>,
) -> Option<ThemeAppearance> {
    match (foreground, background) {
        (Some(fg), Some(bg)) => Some(if bg < fg {
            ThemeAppearance::Dark
        } else {
            ThemeAppearance::Light
        }),
        (None, Some(bg)) => Some(if bg < 0.5 {
            ThemeAppearance::Dark
        } else {
            ThemeAppearance::Light
        }),
        (Some(fg), None) => Some(if fg > 0.5 {
            ThemeAppearance::Dark
        } else {
            ThemeAppearance::Light
        }),
        (None, None) => None,
    }
}

/// `averageLightness` (`theme.ts:206-211`). `theme.ts:200-206`: palette
/// slots below 16 follow the user's palette, so they say nothing about the
/// theme.
fn average_lightness(colors: impl Iterator<Item = Color>) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0.0;
    for color in colors {
        if let Color::Indexed(index) = color
            && index < 16
        {
            continue;
        }
        sum += crate::color::color_to_oklch(color).l;
        count += 1.0;
    }
    if count == 0.0 {
        None
    } else {
        Some(sum / count)
    }
}

// ============================================================================
// Loading and discovery (`theme.ts:551-638`)
// ============================================================================

/// Where themes come from. Configuration, never a global.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemePaths {
    /// The directory holding the built-in themes.
    pub builtin_dir: PathBuf,
    /// pi's `getCustomThemesDir()`: `<agentDir>/themes` (`config.ts:572-574`).
    pub custom_dir: PathBuf,
}

impl ThemePaths {
    /// Paths for the built-in and custom theme directories.
    pub fn new(builtin_dir: impl Into<PathBuf>, custom_dir: impl Into<PathBuf>) -> ThemePaths {
        ThemePaths {
            builtin_dir: builtin_dir.into(),
            custom_dir: custom_dir.into(),
        }
    }

    /// The built-in themes shipped with this crate.
    pub fn builtin_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("themes")
    }

    fn custom_theme_path(&self, name: &str) -> PathBuf {
        self.custom_dir.join(format!("{name}.json"))
    }
}

/// A name and where it came from (`theme.ts:470-472`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    pub path: Option<PathBuf>,
}

/// Themes registered by extensions or loaders (`theme.ts:743-752`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeRegistry {
    entries: BTreeMap<String, Option<PathBuf>>,
}

impl ThemeRegistry {
    /// An empty registry.
    pub fn new() -> ThemeRegistry {
        ThemeRegistry::default()
    }

    /// Register a theme. A registered theme without a source path cannot be
    /// exported (`theme.ts:565-567`).
    pub fn register(&mut self, name: &str, source_path: Option<PathBuf>) -> Result<(), String> {
        assert_theme_name_is_valid(name)?;
        self.entries.insert(name.to_string(), source_path);
        Ok(())
    }

    /// Forget every registered theme.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The registered source path for `name`.
    ///
    /// `None` means unregistered; `Some(None)` is a registered theme without
    /// a file, which cannot be exported.
    pub fn get(&self, name: &str) -> Option<&Option<PathBuf>> {
        self.entries.get(name)
    }

    /// The registered names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
}

/// `getAvailableThemesWithPaths` (`theme.ts:466-498`): `system` first, then the
/// built-ins, then custom themes on disk (invalid ones silently skipped,
/// `theme.ts:500-526`), then registered themes; sorted with `system` hoisted.
pub fn available_themes(paths: &ThemePaths, registry: &ThemeRegistry) -> Vec<ThemeInfo> {
    let mut result = vec![ThemeInfo {
        name: SYSTEM_THEME_NAME.to_string(),
        path: None,
    }];
    let mut seen: BTreeSet<String> = BTreeSet::from([SYSTEM_THEME_NAME.to_string()]);

    for name in BUILTIN_THEME_NAMES {
        if seen.insert(name.to_string()) {
            result.push(ThemeInfo {
                name: name.to_string(),
                path: Some(paths.builtin_dir.join(format!("{name}.json"))),
            });
        }
    }

    if let Ok(entries) = fs::read_dir(&paths.custom_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !seen.insert(stem.to_string()) {
                continue;
            }
            if load_theme_from_path(
                &path,
                TerminalColorMode::TrueColor,
                &TerminalColors::default(),
            )
            .is_err()
            {
                seen.remove(stem);
                continue;
            }
            result.push(ThemeInfo {
                name: stem.to_string(),
                path: Some(path),
            });
        }
    }

    for (name, path) in &registry.entries {
        if seen.insert(name.clone()) {
            result.push(ThemeInfo {
                name: name.clone(),
                path: path.clone(),
            });
        }
    }

    result.sort_by(|left, right| {
        if left.name == SYSTEM_THEME_NAME {
            return std::cmp::Ordering::Less;
        }
        if right.name == SYSTEM_THEME_NAME {
            return std::cmp::Ordering::Greater;
        }
        left.name.cmp(&right.name)
    });
    result
}

/// `loadThemeFromPath` (`theme.ts:608-612`).
pub fn load_theme_from_path(
    path: &Path,
    mode: TerminalColorMode,
    terminal: &TerminalColors,
) -> Result<Theme, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("Failed to parse theme {}: {error}", path.display()))?;
    let json = parse_theme_json_content(&path.display().to_string(), &content)?;
    Theme::from_json(&json, mode, terminal, Some(path.to_path_buf()))
}

/// `loadThemeJson` (`theme.ts:551-574`): built-ins, then registered themes with
/// a source path, then `<customDir>/<name>.json`.
pub fn load_theme_json(
    paths: &ThemePaths,
    registry: &ThemeRegistry,
    name: &str,
) -> Result<ThemeJson, String> {
    if let Some(content) = builtin_content(name) {
        return parse_theme_json_content(name, content);
    }
    if let Some(registered) = registry.get(name) {
        let Some(path) = registered else {
            return Err(format!(
                "Theme \"{name}\" does not have a source path for export"
            ));
        };
        let content = fs::read_to_string(path)
            .map_err(|error| format!("Failed to parse theme {}: {error}", path.display()))?;
        return parse_theme_json_content(&path.display().to_string(), &content);
    }
    let path = paths.custom_theme_path(name);
    if !path.exists() {
        return Err(format!("Theme not found: {name}"));
    }
    let content = fs::read_to_string(&path)
        .map_err(|error| format!("Failed to parse theme {}: {error}", path.display()))?;
    parse_theme_json_content(name, &content)
}

/// The embedded built-in document, when `name` is a built-in.
pub fn builtin_content(name: &str) -> Option<&'static str> {
    match name {
        "dark" => Some(DARK_THEME_JSON),
        "light" => Some(LIGHT_THEME_JSON),
        _ => None,
    }
}

/// `loadTheme` (`theme.ts:625-638`). The `system` name is reserved and takes
/// precedence over a custom theme of the same name; it is generated from the
/// terminal's reported colours rather than read from disk.
pub fn load_theme(
    paths: &ThemePaths,
    registry: &ThemeRegistry,
    name: &str,
    mode: TerminalColorMode,
    terminal: &TerminalColors,
) -> Result<Theme, String> {
    load_theme_with_system(
        paths,
        registry,
        name,
        mode,
        terminal,
        SystemThemeMode::reported(),
    )
}

/// `loadTheme` with the system theme's pending state and appearance hint
/// (`theme.ts:598-611`). A host that knows whether the terminal has answered
/// yet passes them; [`load_theme`] assumes it has.
pub fn load_theme_with_system(
    paths: &ThemePaths,
    registry: &ThemeRegistry,
    name: &str,
    mode: TerminalColorMode,
    terminal: &TerminalColors,
    system: SystemThemeMode,
) -> Result<Theme, String> {
    if name == SYSTEM_THEME_NAME {
        return Ok(generate_system_theme(mode, terminal, system));
    }
    let json = load_theme_json(paths, registry, name)?;
    Theme::from_json(&json, mode, terminal, None)
}

/// `createSystemTheme` (`theme.ts:598-611`): the theme name is the reserved
/// one, its colours come from the terminal, and it carries no export colours.
pub fn generate_system_theme(
    mode: TerminalColorMode,
    terminal: &TerminalColors,
    system: SystemThemeMode,
) -> Theme {
    let mut input = SystemThemeInput::from_terminal(terminal);
    input.saturation = Some(if system.colors_pending { 0.0 } else { 1.0 });
    input.appearance_hint = system.appearance_hint;
    let generated: SystemThemeColors = generate_system_theme_colors(&input);
    Theme::from_colors(
        &generated.colors,
        &generated.backgrounds,
        mode,
        terminal,
        Some(SYSTEM_THEME_NAME.to_string()),
        generated.appearance,
        generated.dim,
    )
}

// ============================================================================
// Settings and terminal detection
// ============================================================================

/// An automatic light/dark theme pair (`theme.ts:655-672`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoThemeSetting {
    pub light_theme: String,
    pub dark_theme: String,
}

/// `theme.ts:655-672`.
pub fn parse_auto_theme_setting(setting: Option<&str>) -> Option<AutoThemeSetting> {
    let setting = setting?;
    let mut parts = setting.split('/');
    let light_theme = parts.next()?.trim();
    let dark_theme = parts.next()?.trim();
    if parts.next().is_some() || light_theme.is_empty() || dark_theme.is_empty() {
        return None;
    }
    Some(AutoThemeSetting {
        light_theme: light_theme.to_string(),
        dark_theme: dark_theme.to_string(),
    })
}

/// `resolveThemeSetting` (`theme.ts:674-685`).
pub fn resolve_theme_setting(
    setting: Option<&str>,
    terminal_theme: ThemeAppearance,
) -> Option<String> {
    if let Some(auto) = parse_auto_theme_setting(setting) {
        return Some(match terminal_theme {
            ThemeAppearance::Light => auto.light_theme,
            ThemeAppearance::Dark => auto.dark_theme,
        });
    }
    match setting {
        Some(value) if value.contains('/') => None,
        Some(value) => Some(value.to_string()),
        None => None,
    }
}

/// `theme.ts:696-702`. Classified by index like Vim: 0-6 and 8 are dark,
/// 7 and 9-15 are light. `value` is `fg;bg` or `fg;xpm;bg`.
pub fn detect_colorfgbg_theme(value: Option<&str>) -> Option<ThemeAppearance> {
    let background = value?.split(';').next_back()?.trim();
    if background.is_empty()
        || background.len() > 2
        || !background.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let index: u32 = background.parse().ok()?;
    if index > 15 {
        return None;
    }
    Some(if index <= 6 || index == 8 {
        ThemeAppearance::Dark
    } else {
        ThemeAppearance::Light
    })
}

/// `theme.ts:705-716`: reported background, then the `CSI ?997` report, then
/// `COLORFGBG`, then dark.
pub fn detect_terminal_theme(
    colors: &TerminalColors,
    reported_scheme: Option<ThemeAppearance>,
    colorfgbg: Option<&str>,
) -> ThemeAppearance {
    if let Some(background) = colors.background {
        return terminal_appearance(background, colors.foreground);
    }
    reported_scheme
        .or_else(|| detect_colorfgbg_theme(colorfgbg))
        .unwrap_or(ThemeAppearance::Dark)
}

/// sRGB channel linearisation (`system-theme.ts:390-393`).
pub fn linear_channel(channel: f64) -> f64 {
    let value = channel / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance (`system-theme.ts:395-398`).
pub fn relative_luminance(color: Rgb) -> f64 {
    0.2126 * linear_channel(color.r)
        + 0.7152 * linear_channel(color.g)
        + 0.0722 * linear_channel(color.b)
}

/// WCAG contrast ratio, 1..21 (`system-theme.ts:400-404`).
pub fn wcag_contrast(first: Rgb, second: Rgb) -> f64 {
    let a = relative_luminance(first);
    let b = relative_luminance(second);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// `OKLab` lightness, 0..1 (`system-theme.ts:406-408`). pi reads it as
/// `colorToOklch(...).l`, which is the raw Oklab `L` - *not* the OKHSL
/// lightness of [`crate::oklab::oklab_to_okhsl_lightness`]. The contrast
/// curves in [`crate::system_theme`] are fitted against this one.
pub fn oklab_lightness(color: Rgb) -> f64 {
    rgb_to_oklab(color)[0]
}

/// `terminalAppearance` (`system-theme.ts:416-431`).
pub fn terminal_appearance(background: Rgb, foreground: Option<Rgb>) -> ThemeAppearance {
    let black = Rgb {
        r: 0.0,
        g: 0.0,
        b: 0.0,
    };
    let white_contrast = wcag_contrast(WHITE_RGB, background);
    let black_contrast = wcag_contrast(black, background);
    if let Some(foreground) = foreground {
        let difference = (oklab_lightness(foreground) - oklab_lightness(background)).abs();
        if difference > 0.05 {
            let candidate = if oklab_lightness(foreground) > oklab_lightness(background) {
                ThemeAppearance::Dark
            } else {
                ThemeAppearance::Light
            };
            let contrast = match candidate {
                ThemeAppearance::Dark => white_contrast,
                ThemeAppearance::Light => black_contrast,
            };
            if contrast >= TEXT_MINIMUM_WCAG_CONTRAST {
                return candidate;
            }
        }
    }
    if white_contrast >= black_contrast {
        ThemeAppearance::Dark
    } else {
        ThemeAppearance::Light
    }
}

// ============================================================================
// File watcher (`theme.ts:811-897`)
// ============================================================================

/// A poll-based stand-in for pi's `fs.watch` + 100 ms debounce timer. No thread,
/// no timer and no clock live here: the host calls
/// [`ThemeWatcher::should_reload`] from its own loop with a millisecond reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeWatcher {
    theme_name: String,
    path: PathBuf,
    seen_modified: Option<SystemTime>,
    pending_since_ms: Option<u64>,
}

impl ThemeWatcher {
    /// `startThemeWatcher` (`theme.ts:814-833`): no watcher for the built-ins
    /// or `system`, and none when the theme file does not exist.
    pub fn new(theme_name: &str, custom_dir: &Path) -> Option<ThemeWatcher> {
        if theme_name.is_empty()
            || BUILTIN_THEME_NAMES.contains(&theme_name)
            || theme_name == SYSTEM_THEME_NAME
        {
            return None;
        }
        let path = custom_dir.join(format!("{theme_name}.json"));
        if !path.exists() {
            return None;
        }
        let seen_modified = fs::metadata(&path).ok()?.modified().ok();
        Some(ThemeWatcher {
            theme_name: theme_name.to_string(),
            path,
            seen_modified,
            pending_since_ms: None,
        })
    }

    /// The theme this watcher polls.
    pub fn theme_name(&self) -> &str {
        &self.theme_name
    }

    /// The watched file. pi watches the *directory* (`theme.ts:824-828`); this
    /// port polls the file itself because the host owns the directory scan.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A change is waiting out the debounce window.
    pub fn is_pending(&self) -> bool {
        self.pending_since_ms.is_some()
    }

    /// `stopThemeWatcher` (`theme.ts:891-897`).
    pub fn stop(&mut self) {
        self.pending_since_ms = None;
        self.seen_modified = None;
    }

    /// True once a changed file has been quiet for [`THEME_WATCH_DEBOUNCE_MS`].
    /// Each new change re-arms the window (`theme.ts:835-841`); a vanished file
    /// keeps the last good theme (`theme.ts:843-861`).
    pub fn should_reload(&mut self, now_ms: u64) -> bool {
        let current = fs::metadata(&self.path)
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        if current != self.seen_modified {
            self.seen_modified = current;
            self.pending_since_ms = if current.is_some() {
                Some(now_ms)
            } else {
                None
            };
            return false;
        }
        if let Some(since) = self.pending_since_ms
            && now_ms.saturating_sub(since) >= THEME_WATCH_DEBOUNCE_MS
        {
            self.pending_since_ms = None;
            return true;
        }
        false
    }
}

/// Settings-to-theme wiring in the spirit of `theme-controller.ts`: it owns the
/// active [`Theme`] and the watcher, with no global state.
#[derive(Debug)]
pub struct ThemeController {
    paths: ThemePaths,
    registry: ThemeRegistry,
    mode: TerminalColorMode,
    terminal: TerminalColors,
    system: SystemThemeMode,
    setting: Option<String>,
    theme: Theme,
    theme_name: String,
    watcher: Option<ThemeWatcher>,
}

impl ThemeController {
    /// Load `name` and take ownership of its watcher.
    ///
    /// Like [`ThemeController::new_with_system`] with
    /// [`SystemThemeMode::reported`]: the terminal is assumed to have answered
    /// its colour report.
    pub fn new(
        paths: ThemePaths,
        registry: ThemeRegistry,
        name: &str,
        mode: TerminalColorMode,
        terminal: TerminalColors,
    ) -> Result<ThemeController, String> {
        Self::new_with_system(
            paths,
            registry,
            name,
            mode,
            terminal,
            SystemThemeMode::reported(),
        )
    }

    /// As [`ThemeController::new`], for a host that knows whether the terminal
    /// has reported its colours yet (`theme-controller.ts:86-89`).
    pub fn new_with_system(
        paths: ThemePaths,
        registry: ThemeRegistry,
        name: &str,
        mode: TerminalColorMode,
        terminal: TerminalColors,
        system: SystemThemeMode,
    ) -> Result<ThemeController, String> {
        let theme = load_theme_with_system(&paths, &registry, name, mode, &terminal, system)?;
        let watcher = ThemeWatcher::new(name, &paths.custom_dir);
        Ok(ThemeController {
            paths,
            registry,
            mode,
            terminal,
            system,
            setting: Some(name.to_string()),
            theme,
            theme_name: name.to_string(),
            watcher,
        })
    }

    /// The active theme.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// The active theme's name, after any `light/dark` setting was resolved.
    pub fn theme_name(&self) -> &str {
        &self.theme_name
    }

    /// `setThemeSetting` (`theme-controller.ts:135-138`): a `light/dark` pair
    /// re-resolves per terminal appearance.
    pub fn set_theme_setting(&mut self, setting: &str) -> Result<(), String> {
        let resolved = resolve_theme_setting(
            Some(setting),
            detect_terminal_theme(&self.terminal, None, None),
        )
        .ok_or_else(|| format!("Invalid theme setting: {setting}"))?;
        self.apply_name(&resolved)
    }

    fn apply_name(&mut self, name: &str) -> Result<(), String> {
        let theme = load_theme_with_system(
            &self.paths,
            &self.registry,
            name,
            self.mode,
            &self.terminal,
            self.system,
        )?;
        self.theme = theme;
        self.theme_name = name.to_string();
        self.setting = Some(name.to_string());
        self.watcher = ThemeWatcher::new(name, &self.paths.custom_dir);
        Ok(())
    }

    /// Record colours the terminal reported; tokens set to `""` pick them up.
    pub fn set_terminal_colors(&mut self, terminal: TerminalColors) {
        self.terminal = terminal;
    }

    /// Whether a colour report is still outstanding, which is what keeps the
    /// system theme in grayscale (`theme.ts:190`, `theme-controller.ts:88`).
    pub fn set_system_theme_mode(&mut self, system: SystemThemeMode) {
        self.system = system;
    }

    /// True when the watcher fired and a reloaded theme was installed.
    pub fn should_reload(&mut self, now_ms: u64) -> bool {
        let Some(watcher) = self.watcher.as_mut() else {
            return false;
        };
        // Stale-name guard (`theme.ts:865-869`).
        if watcher.theme_name() != self.theme_name {
            return false;
        }
        if !watcher.should_reload(now_ms) {
            return false;
        }
        // Mid-edit state is ignored (`theme.ts:857-861`).
        load_theme_from_path(
            watcher.path().to_path_buf().as_path(),
            self.mode,
            &self.terminal,
        )
        .is_ok_and(|theme| {
            self.theme = theme;
            true
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{okhsl_color, oklch_color};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique directory under the system temp dir; removed on drop so no
    /// test writes into a real agent directory.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> TempDir {
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pi-tui-theme-{}-{}-{}",
                std::process::id(),
                unique,
                label
            ));
            fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }

        fn write(&self, name: &str, content: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, content).expect("write theme");
            path
        }

        /// Move a file's mtime forward deterministically. Filesystem timestamp
        /// granularity would otherwise make the watcher tests depend on how
        /// fast the machine writes.
        fn touch(&self, name: &str) {
            let file = fs::File::open(self.0.join(name)).expect("open for touch");
            let bumped = file
                .metadata()
                .expect("metadata")
                .modified()
                .expect("mtime")
                + std::time::Duration::from_secs(60);
            file.set_modified(bumped).expect("set mtime");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode() -> TerminalColorMode {
        TerminalColorMode::TrueColor
    }

    fn terminal() -> TerminalColors {
        TerminalColors::default()
    }

    fn minimal_theme_json() -> String {
        let entries: Vec<String> = required_color_tokens()
            .iter()
            .map(|token| format!("\"{token}\": \"#888888\""))
            .collect();
        format!(
            "{{\"name\":\"minimal\",\"colors\":{{{}}}}}",
            entries.join(",")
        )
    }

    fn complete_document(name: &str) -> Value {
        let mut colors = serde_json::Map::new();
        for token in required_color_tokens() {
            colors.insert(token.to_string(), Value::String("#888888".into()));
        }
        serde_json::json!({ "name": name, "colors": Value::Object(colors) })
    }

    // --- token lists -------------------------------------------------------

    #[test]
    fn token_counts_match_pi() {
        assert_eq!(ThemeColor::ALL.len(), 49);
        assert_eq!(ThemeBg::ALL.len(), 7);
        assert_eq!(required_color_tokens().len(), 51);
        assert_eq!(
            ThemeBg::ALL
                .iter()
                .filter(|token| !token.is_optional())
                .count(),
            6
        );
    }

    #[test]
    fn token_names_round_trip() {
        for token in ThemeColor::ALL {
            assert_eq!(ThemeColor::from_name(token.as_str()), Some(*token));
        }
        for token in ThemeBg::ALL {
            assert_eq!(ThemeBg::from_name(token.as_str()), Some(*token));
        }
        assert_eq!(ThemeColor::from_name("selectedBg"), None);
        assert_eq!(ThemeBg::from_name("accent"), None);
    }

    // --- colour values -----------------------------------------------------

    #[test]
    fn parses_colour_grammars() {
        assert_eq!(parse_color("#ff0000"), Some(rgb_color(255.0, 0.0, 0.0)));
        assert_eq!(parse_color("#f00"), Some(rgb_color(255.0, 0.0, 0.0)));
        // pi's parseColor knows only hex, oklch() and okhsl() (colors.ts:121-148).
        assert_eq!(parse_color("rgb:ff/00/00"), None);
        assert!(parse_color("okhsl(20 72% 67%)").is_some());
        assert!(parse_color("oklch(0.5 0.2 30)").is_some());
        assert_eq!(parse_color("nonsense"), None);
    }

    #[test]
    fn okhsl_and_oklch_round_trip_through_shared_converters() {
        let expected = okhsl_color(20.0, 0.72, 0.67);
        assert_eq!(parse_color("okhsl(20 72% 67%)"), Some(expected));
        // Without the `%` pi passes the components through unscaled
        // (`colors.ts:143`), and 72 is out of the 0-1 range it checks, so
        // pi throws - which here is `None`.
        assert_eq!(parse_color("okhsl(20 72 67)"), None);
        assert_eq!(
            parse_color("oklch(50% 0.2 20)"),
            Some(oklch_color(0.5, 0.2, 20.0))
        );
    }

    // --- built-in themes ---------------------------------------------------

    #[test]
    fn builtin_documents_parse_and_every_token_resolves() {
        for name in BUILTIN_THEME_NAMES {
            let json = ThemeJson::builtin(name).expect(name);
            assert_eq!(json.name, name);
            assert_eq!(
                json.appearance.map(ThemeAppearance::as_str),
                Some(name),
                "{name} declares its appearance"
            );
            let theme = Theme::from_json(&json, mode(), &terminal(), None).expect(name);
            for token in ThemeColor::ALL {
                assert!(
                    json.colors.contains_key(token.as_str()),
                    "{name} is missing {}",
                    token.as_str()
                );
                let _ = theme.fg_color(*token);
            }
            for token in ThemeBg::ALL {
                assert!(
                    json.colors.contains_key(token.as_str()),
                    "{name} is missing {}",
                    token.as_str()
                );
                let _ = theme.bg_color(*token);
            }
            assert_eq!(
                theme.appearance().as_str(),
                name,
                "{name} resolves to its own appearance"
            );
            let export = theme.export_colors();
            assert!(export.page_bg.is_some() && export.card_bg.is_some());
        }
    }

    #[test]
    fn vars_resolve_through_references() {
        let document = serde_json::json!({
            "name": "vars",
            "vars": { "a": "#112233", "b": "a" },
            "colors": Value::Object(
                required_color_tokens()
                    .iter()
                    .map(|token| (token.to_string(), Value::String("b".into())))
                    .collect::<serde_json::Map<_, _>>(),
            ),
        });
        let json = validate_theme_json("vars", &document).expect("valid");
        let theme = Theme::from_json(&json, mode(), &terminal(), None).expect("theme");
        assert_eq!(
            theme.fg_color(ThemeColor::Text),
            rgb_color(0x11 as f64, 0x22 as f64, 0x33 as f64)
        );
    }

    #[test]
    fn unknown_and_circular_var_references_are_reported() {
        let mut vars = BTreeMap::new();
        vars.insert("missing".to_string(), ColorValue::Text("nope".into()));
        let err = ColorValue::Text("missing".into())
            .resolve(&vars)
            .expect_err("missing");
        assert_eq!(err, "Variable reference not found: nope");

        let mut vars = BTreeMap::new();
        vars.insert("loop".into(), ColorValue::Text("other".into()));
        vars.insert("other".into(), ColorValue::Text("loop".into()));
        let err = ColorValue::Text("loop".into())
            .resolve(&vars)
            .expect_err("cycle");
        // The message names the variable whose resolution re-entered itself
        // (`theme.ts:144`), not the one it was pointing at.
        assert_eq!(err, "Circular variable reference detected: loop");
    }

    #[test]
    fn optional_tokens_fall_back() {
        let mut colors = serde_json::Map::new();
        for token in required_color_tokens() {
            colors.insert(token.to_string(), Value::String("#888888".into()));
        }
        // Drop every optional token so the fallbacks fire.
        for token in ThemeColor::ALL.iter().filter(|t| t.is_optional()) {
            colors.remove(token.as_str());
        }
        colors.remove("searchMatchBg");
        let document = serde_json::json!({ "name": "fallback", "colors": Value::Object(colors) });
        let json = validate_theme_json("fallback", &document).expect("valid");
        assert!(!json.colors.contains_key("thinkingMax"));
        let theme = Theme::from_json(&json, mode(), &terminal(), None).expect("theme");
        assert_eq!(
            theme.fg_color(ThemeColor::ScrollbarTrack),
            rgb_color(136.0, 136.0, 136.0)
        );
        assert_eq!(
            theme.fg_color(ThemeColor::ThinkingMax),
            theme.fg_color(ThemeColor::ThinkingXhigh)
        );
        assert_eq!(
            theme.bg_color(ThemeBg::SearchMatchBg),
            theme.bg_color(ThemeBg::SelectedBg)
        );
        assert_eq!(
            theme.fg_color(ThemeColor::SearchMatchText),
            theme.fg_color(ThemeColor::Text)
        );
    }

    #[test]
    fn empty_tokens_use_the_terminal_default_for_their_own_slot() {
        let mut colors = serde_json::Map::new();
        for token in required_color_tokens() {
            colors.insert(token.to_string(), Value::String("#888888".into()));
        }
        colors.insert("accent".into(), Value::String(String::new()));
        colors.insert("selectedBg".into(), Value::String(String::new()));
        let document = serde_json::json!({ "name": "defaults", "colors": Value::Object(colors) });
        let json = validate_theme_json("defaults", &document).expect("valid");
        let terminal = TerminalColors {
            foreground: Some(Rgb {
                r: 1.0,
                g: 2.0,
                b: 3.0,
            }),
            background: Some(Rgb {
                r: 250.0,
                g: 251.0,
                b: 252.0,
            }),
            palette: None,
        };
        let theme = Theme::from_json(&json, mode(), &terminal, None).expect("theme");
        assert_eq!(theme.fg_ansi(ThemeColor::Accent), "\u{1b}[39m");
        assert_eq!(theme.bg_ansi(ThemeBg::SelectedBg), "\u{1b}[49m");
        assert_eq!(theme.fg(ThemeColor::Accent, "hi"), "\u{1b}[39mhi\u{1b}[39m");
        assert_eq!(theme.fg_color(ThemeColor::Accent), rgb_color(1.0, 2.0, 3.0));
        assert_eq!(
            theme.bg_color(ThemeBg::SelectedBg),
            rgb_color(250.0, 251.0, 252.0)
        );
    }

    #[test]
    fn dim_tokens_are_mixed_toward_the_background() {
        let json = ThemeJson::builtin("dark").expect("dark");
        let theme = Theme::from_json(&json, mode(), &terminal(), None)
            .expect("theme")
            .with_dim_tokens(vec![ThemeColor::Muted]);
        let dim = theme.fg_color(ThemeColor::Muted);
        assert_ne!(dim, theme.fg_color(ThemeColor::Border));
        assert!(theme.fg(ThemeColor::Muted, "x").contains("\u{1b}[2m"));
    }

    // --- schema validation -------------------------------------------------

    #[test]
    fn missing_tokens_produce_the_verbatim_message() {
        let document = serde_json::json!({
            "name": "broken",
            "colors": { "accent": "#ffffff", "error": 12 }
        });
        let err = validate_theme_json("broken", &document).expect_err("invalid");
        let mut expected =
            String::from("Invalid theme \"broken\":\n\nMissing required color tokens:\n");
        let mut missing: Vec<&str> = required_color_tokens()
            .into_iter()
            .filter(|token| *token != "accent" && *token != "error")
            .collect();
        missing.sort_unstable();
        for token in missing {
            {
                use std::fmt::Write as _;
                let _ = writeln!(expected, "  - {token}");
            }
        }
        expected.push_str(
            "\nPlease add these colors to your theme's \"colors\" object.\nSee the built-in themes (dark.json, light.json) for reference values.",
        );
        assert_eq!(err, expected);
    }

    #[test]
    fn other_errors_are_listed_verbatim() {
        let document = serde_json::json!({
            "name": "mixed",
            "appearance": "blue",
            "vars": { "x": true },
            "colors": { "accent": "#fff", "muted": 300 }
        });
        let err = validate_theme_json("mixed", &document).expect_err("invalid");
        assert!(err.starts_with("Invalid theme \"mixed\":\n\nMissing required color tokens:\n"));
        assert!(err.ends_with(
            "\n\nOther errors:\n  - /appearance: Expected \"dark\" or \"light\"\n  - /vars/x: Expected a string or an integer between 0 and 255, got a boolean\n  - /colors/muted: Expected an integer between 0 and 255"
        ));
    }

    #[test]
    fn unknown_keys_are_accepted() {
        let mut document = complete_document("extra");
        document["colors"]["customToken"] = Value::String("#010203".into());
        document["extraTopLevel"] = Value::Bool(true);
        assert!(validate_theme_json("extra", &document).is_ok());
    }

    #[test]
    fn invalid_shapes_and_names_use_pi_wording() {
        let err = validate_theme_json("x", &Value::Bool(true)).expect_err("invalid");
        assert_eq!(
            err,
            "Invalid theme \"x\": expected an object with a \"colors\" map."
        );
        let err = parse_theme_json_loose("x", &Value::Bool(true)).expect_err("invalid");
        assert_eq!(
            err,
            "Invalid theme \"x\": expected an object with a \"colors\" map."
        );
        assert!(parse_theme_json_loose("x", &Value::Bool(false)).is_err());

        let mut document = complete_document("a/b");
        document["name"] = Value::String("a/b".into());
        let err = validate_theme_json("x", &document).expect_err("invalid");
        assert_eq!(
            err,
            "Invalid theme name \"a/b\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
        );
        assert_eq!(
            assert_theme_name_is_valid("a/b").expect_err("slash"),
            "Invalid theme name \"a/b\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
        );
    }

    #[test]
    fn parse_failures_carry_the_theme_label() {
        let err = parse_theme_json_content("my.json", "{ nope").expect_err("parse");
        assert!(err.starts_with("Failed to parse theme my.json: "), "{err}");
        // A BOM is stripped before parsing (`theme.ts:549-558`).
        let document = minimal_theme_json();
        let json = parse_theme_json_content("bom", &format!("\u{feff}{document}")).expect("ok");
        assert_eq!(json.name, "minimal");
    }

    #[test]
    fn integer_indices_come_back_as_palette_colors() {
        let mut document = complete_document("indexed");
        document["colors"]["accent"] = Value::from(200);
        let json = validate_theme_json("indexed", &document).expect("valid");
        let theme = Theme::from_json(&json, mode(), &terminal(), None).expect("theme");
        assert_eq!(theme.fg_color(ThemeColor::Accent), indexed_color(200));
    }

    // --- discovery order ---------------------------------------------------

    #[test]
    fn discovery_order_prefers_builtins_then_registry_then_custom() {
        let dir = TempDir::new("discovery");
        let builtin = TempDir::new("builtin");
        let paths = ThemePaths::new(builtin.0.clone(), dir.0.clone());
        // A custom `dark` must not shadow the built-in.
        dir.write("dark.json", "{\"name\":\"shadow\"}");
        dir.write("mine.json", &minimal_theme_json());

        let registry = ThemeRegistry::new();
        let theme = load_theme(&paths, &registry, "dark", mode(), &terminal()).expect("dark");
        assert_eq!(theme.name(), Some("dark"));
        assert_eq!(
            theme.fg_color(ThemeColor::Text),
            ThemeJson::builtin("dark")
                .map(|json| Theme::from_json(&json, mode(), &terminal(), None)
                    .unwrap()
                    .fg_color(ThemeColor::Text))
                .unwrap()
        );

        let custom = load_theme(&paths, &registry, "mine", mode(), &terminal()).expect("mine");
        // The display name is the JSON `name` field (`theme.ts:604`); the file
        // only decides *which* theme was loaded.
        assert_eq!(custom.name(), Some("minimal"));

        let err = load_theme(&paths, &registry, "absent", mode(), &terminal()).expect_err("absent");
        assert_eq!(err, "Theme not found: absent");

        let mut registry = registry;
        registry.register("mine", None).expect("register");
        let err = load_theme(&paths, &registry, "mine", mode(), &terminal()).expect_err("no path");
        assert_eq!(err, "Theme \"mine\" does not have a source path for export");
    }

    #[test]
    fn the_reserved_system_name_generates_and_is_not_shadowed() {
        let dir = TempDir::new("system");
        let paths = ThemePaths::new(TempDir::new("builtin2").0.clone(), dir.0.clone());
        dir.write("system.json", &minimal_theme_json());
        let registry = ThemeRegistry::new();
        // The reserved name wins over a custom theme file of the same name
        // (`theme.ts:629`): what loads is the generated one.
        let theme =
            load_theme(&paths, &registry, SYSTEM_THEME_NAME, mode(), &terminal()).expect("system");
        assert_eq!(theme.name(), Some(SYSTEM_THEME_NAME));
        assert_eq!(theme.source_path(), None);
        assert_eq!(theme.fg_ansi(ThemeColor::Text), "\u{1b}[39m");
    }

    #[test]
    fn available_themes_hoists_system_and_skips_invalid_files() {
        let dir = TempDir::new("listing");
        let paths = ThemePaths::new(TempDir::new("builtin3").0.clone(), dir.0.clone());
        dir.write("zebra.json", &minimal_theme_json());
        dir.write("broken.json", "{\"name\":\"broken\"}");
        dir.write("ignored.txt", "not a theme");
        let mut registry = ThemeRegistry::new();
        registry.register("registered", None).expect("register");
        let names: Vec<String> = available_themes(&paths, &registry)
            .into_iter()
            .map(|info| info.name)
            .collect();
        assert_eq!(
            names,
            vec!["system", "dark", "light", "registered", "zebra"]
        );
    }

    #[test]
    fn source_paths_are_recorded() {
        let dir = TempDir::new("source");
        let path = dir.write("mine.json", &minimal_theme_json());
        let theme = load_theme_from_path(&path, mode(), &terminal()).expect("load");
        assert_eq!(theme.name(), Some("minimal"));
        assert!(theme.source_path().is_some());
    }

    // --- settings ----------------------------------------------------------

    #[test]
    fn auto_theme_setting_parsing() {
        assert_eq!(
            parse_auto_theme_setting(Some("light/dark")),
            Some(AutoThemeSetting {
                light_theme: "light".into(),
                dark_theme: "dark".into()
            })
        );
        assert_eq!(
            parse_auto_theme_setting(Some(" solar / moon "))
                .unwrap()
                .dark_theme,
            "moon"
        );
        assert_eq!(parse_auto_theme_setting(Some("a/b/c")), None);
        assert_eq!(parse_auto_theme_setting(Some("/dark")), None);
        assert_eq!(parse_auto_theme_setting(Some("light/")), None);
        assert_eq!(parse_auto_theme_setting(Some("plain")), None);
        assert_eq!(parse_auto_theme_setting(None), None);
    }

    #[test]
    fn theme_setting_resolution_by_appearance() {
        assert_eq!(
            resolve_theme_setting(Some("light/dark"), ThemeAppearance::Dark).as_deref(),
            Some("dark")
        );
        assert_eq!(
            resolve_theme_setting(Some("light/dark"), ThemeAppearance::Light).as_deref(),
            Some("light")
        );
        assert_eq!(
            resolve_theme_setting(Some("solar"), ThemeAppearance::Light).as_deref(),
            Some("solar")
        );
        assert_eq!(
            resolve_theme_setting(Some("a/b/c"), ThemeAppearance::Light),
            None
        );
        assert_eq!(resolve_theme_setting(None, ThemeAppearance::Light), None);
    }

    // --- terminal detection ------------------------------------------------

    #[test]
    fn colorfgbg_classification() {
        for dark in ["15;0", "0;15;0", "default;0", "  0  "] {
            assert_eq!(
                detect_colorfgbg_theme(Some(dark)),
                Some(ThemeAppearance::Dark),
                "{dark}"
            );
        }
        assert_eq!(
            detect_colorfgbg_theme(Some("0;15")),
            Some(ThemeAppearance::Light)
        );
        assert_eq!(
            detect_colorfgbg_theme(Some("0;8")),
            Some(ThemeAppearance::Dark)
        );
        assert_eq!(detect_colorfgbg_theme(Some("0;16")), None);
        assert_eq!(detect_colorfgbg_theme(Some("0;default")), None);
        assert_eq!(detect_colorfgbg_theme(Some("0;999")), None);
        assert_eq!(detect_colorfgbg_theme(None), None);
    }

    #[test]
    fn detection_precedence() {
        let reported = TerminalColors {
            background: Some(WHITE_RGB),
            foreground: Some(Rgb {
                r: 0.0,
                g: 0.0,
                b: 0.0,
            }),
            palette: None,
        };
        // A reported background wins over everything else.
        assert_eq!(
            detect_terminal_theme(&reported, Some(ThemeAppearance::Dark), Some("15;0")),
            ThemeAppearance::Light
        );
        // Then the scheme report, then COLORFGBG, then dark.
        assert_eq!(
            detect_terminal_theme(
                &TerminalColors::default(),
                Some(ThemeAppearance::Light),
                Some("15;0")
            ),
            ThemeAppearance::Light
        );
        assert_eq!(
            detect_terminal_theme(&TerminalColors::default(), None, Some("15;0")),
            ThemeAppearance::Dark
        );
        assert_eq!(
            detect_terminal_theme(&TerminalColors::default(), None, None),
            ThemeAppearance::Dark
        );
    }

    #[test]
    fn wcag_luminance_and_contrast() {
        let black = Rgb {
            r: 0.0,
            g: 0.0,
            b: 0.0,
        };
        assert!((relative_luminance(black) - 0.0).abs() < 1e-12);
        assert!((relative_luminance(WHITE_RGB) - 1.0).abs() < 1e-12);
        assert!((linear_channel(10.0) - 0.003_035_269_8).abs() < 1e-9);
        assert!((linear_channel(255.0) - 1.0).abs() < 1e-12);
        assert!((wcag_contrast(WHITE_RGB, black) - 21.0).abs() < 1e-9);
        assert!((wcag_contrast(black, black) - 1.0).abs() < 1e-9);
        let ratio = wcag_contrast(
            Rgb {
                r: 18.0,
                g: 18.0,
                b: 18.0,
            },
            WHITE_RGB,
        );
        assert!(ratio > TEXT_MINIMUM_WCAG_CONTRAST, "{ratio}");
        assert!((oklab_lightness(WHITE_RGB) - 1.0).abs() < 1e-3);
        assert!(oklab_lightness(black).abs() < 1e-3);
    }

    #[test]
    fn terminal_appearance_uses_foreground_when_it_is_readable() {
        let dark_bg = Rgb {
            r: 18.0,
            g: 18.0,
            b: 18.0,
        };
        let light_bg = Rgb {
            r: 240.0,
            g: 240.0,
            b: 240.0,
        };
        assert_eq!(
            terminal_appearance(dark_bg, Some(WHITE_RGB)),
            ThemeAppearance::Dark
        );
        assert_eq!(
            terminal_appearance(
                light_bg,
                Some(Rgb {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0
                })
            ),
            ThemeAppearance::Light
        );
        // No foreground: whichever of black / white contrasts more.
        assert_eq!(terminal_appearance(dark_bg, None), ThemeAppearance::Dark);
        assert_eq!(terminal_appearance(light_bg, None), ThemeAppearance::Light);
        // A foreground that differs too little falls back to the better of black
        // / white contrast, which mid grey does not give to white.
        assert_eq!(
            terminal_appearance(
                Rgb {
                    r: 130.0,
                    g: 130.0,
                    b: 130.0
                },
                Some(Rgb {
                    r: 131.0,
                    g: 131.0,
                    b: 131.0
                })
            ),
            ThemeAppearance::Light
        );
    }

    // --- styling -----------------------------------------------------------

    #[test]
    fn styling_helpers_emit_escapes() {
        let json = ThemeJson::builtin("dark").expect("dark");
        let theme = Arc::new(Theme::from_json(&json, mode(), &terminal(), None).expect("theme"));
        let styled = theme.fg(ThemeColor::Error, "boom");
        assert!(styled.starts_with('\u{1b}'));
        assert!(styled.ends_with("\u{1b}[39m"));
        let background = theme.bg(ThemeBg::ToolErrorBg, "boom");
        assert!(background.ends_with("\u{1b}[49m"));
        let styled = theme.style(
            "x",
            ThemeStyle {
                fg: Some(ThemeColor::Accent),
                bg: Some(ThemeBg::SelectedBg),
                attributes: TextAttributes {
                    bold: true,
                    ..TextAttributes::default()
                },
            },
        );
        assert!(styled.contains("\u{1b}[1m"));
        let markdown = theme.markdown_theme();
        assert!((markdown.heading)("h").contains('\u{1b}'));
        assert!((markdown.bold)("b").contains("\u{1b}[1m"));
        assert!(markdown.highlight_code.is_some());
    }

    // --- watcher -----------------------------------------------------------

    #[test]
    fn watcher_is_only_started_for_custom_themes_that_exist() {
        let dir = TempDir::new("watch-none");
        assert!(ThemeWatcher::new("dark", &dir.0).is_none());
        assert!(ThemeWatcher::new("light", &dir.0).is_none());
        assert!(ThemeWatcher::new(SYSTEM_THEME_NAME, &dir.0).is_none());
        assert!(ThemeWatcher::new("", &dir.0).is_none());
        assert!(ThemeWatcher::new("mine", &dir.0).is_none());
        dir.write("mine.json", &minimal_theme_json());
        let watcher = ThemeWatcher::new("mine", &dir.0).expect("watcher");
        assert_eq!(watcher.theme_name(), "mine");
        assert_eq!(watcher.path(), dir.0.join("mine.json"));
        assert!(!watcher.is_pending());
    }

    #[test]
    fn watcher_debounces_before_reloading() {
        let dir = TempDir::new("watch-debounce");
        let path = dir.write("mine.json", &minimal_theme_json());
        let mut watcher = ThemeWatcher::new("mine", &dir.0).expect("watcher");
        assert!(!watcher.should_reload(0));
        assert!(!watcher.should_reload(1_000));
        // A changed file arms the window instead of reloading immediately.
        dir.write("mine.json", &minimal_theme_json());
        dir.touch("mine.json");
        assert!(!watcher.should_reload(2_000));
        assert!(watcher.is_pending());
        // Each event re-arms the 100 ms window.
        assert!(!watcher.should_reload(2_050));
        assert!(!watcher.should_reload(2_099));
        assert!(watcher.should_reload(2_100));
        assert!(!watcher.is_pending());
        // Fires only once per change.
        assert!(!watcher.should_reload(3_000));
        let _ = path;
    }

    #[test]
    fn watcher_keeps_the_last_theme_when_the_file_disappears() {
        let dir = TempDir::new("watch-delete");
        dir.write("mine.json", &minimal_theme_json());
        let mut watcher = ThemeWatcher::new("mine", &dir.0).expect("watcher");
        fs::remove_file(dir.0.join("mine.json")).expect("remove");
        assert!(!watcher.should_reload(0));
        assert!(!watcher.is_pending());
        assert!(!watcher.should_reload(1_000));
        watcher.stop();
        assert!(watcher.pending_since_ms.is_none());
    }

    #[test]
    fn controller_swaps_theme_and_reloads_on_change() {
        let dir = TempDir::new("controller");
        let builtin = TempDir::new("controller-builtin");
        let paths = ThemePaths::new(builtin.0.clone(), dir.0.clone());
        dir.write("mine.json", &minimal_theme_json());
        let mut controller =
            ThemeController::new(paths, ThemeRegistry::new(), "mine", mode(), terminal())
                .expect("controller");
        assert_eq!(controller.theme_name(), "mine");
        // No reported colours, so the pair resolves its dark half.
        controller.set_theme_setting("light/dark").expect("setting");
        assert_eq!(controller.theme_name(), "dark");
        assert!(controller.watcher.is_none());
        controller.set_theme_setting("mine").expect("setting");
        assert_eq!(controller.theme_name(), "mine");
        assert!(controller.watcher.is_some());

        dir.write("mine.json", &minimal_theme_json());
        dir.touch("mine.json");
        assert!(!controller.should_reload(0));
        assert!(controller.should_reload(THEME_WATCH_DEBOUNCE_MS));
    }
}
