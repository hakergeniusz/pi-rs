//! The `system` theme: pi's colors derived from the terminal's own theme.
//!
//! Ground truth: `packages/coding-agent/src/modes/interactive/theme/system-theme.ts`
//! (656 lines). Every literal below is that file's: the color families and their
//! ANSI slots (`:41-59`), the token/family table (`:61-133`), the contrast level
//! curves (`:143-241`), the rules (`:246-345`), the solve order (`:363-377`),
//! the public API (`:379-431`), the generation (`:461-573`) and the indexed
//! fallback for terminals that reported nothing (`:648-656`).
//!
//! Every token belongs to a color family (its hue) and has contrast rules: it
//! must reach a contrast level on the background and on the panels it is drawn
//! on. Hue and saturation come from the terminal's palette color for the
//! family's ANSI slot, or from the family's own hue when the terminal reports no
//! palette. Lightness comes from the rules alone. Colors are built in OKHSL,
//! whose saturation is relative to the sRGB gamut.
//!
//! Depending on what the terminal reports, the theme is generated in one of
//! three tiers:
//!
//! - background and palette: hues from the palette, lightness from the
//!   background;
//! - background only: the families' own hues, lightness from the background;
//! - nothing: ANSI palette indices and the default colors, which the terminal
//!   renders itself.
//!
//! Nothing here reads a terminal, an environment variable or a clock: the
//! caller hands in the colors the terminal reported.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::color::{
    TerminalColors, color_to_hex, color_to_oklch, color_to_rgb, oklch_color, rgb_color,
};
use crate::oklab::{Rgb, okhsl_to_rgb, oklab_to_okhsl_lightness, rgb_to_okhsl};
use crate::theme::{
    ColorValue, TEXT_MINIMUM_WCAG_CONTRAST, ThemeAppearance, ThemeBg, ThemeColor, oklab_lightness,
    terminal_appearance, wcag_contrast,
};

// ============================================================================
// Recipe: color families and their tokens
// ============================================================================

// A family's OKHSL hue and saturation: `max` at mid lightness, falling toward
// `min` at black and white, plus the ANSI palette slot the family takes its
// hue and saturation from. `system-theme.ts:41-47`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Family {
    hue: f64,
    min: f64,
    max: f64,
    slot: u8,
}

// `FAMILIES` (`system-theme.ts:49-64`).
const FAMILIES: [(&str, Family); 14] = [
    (
        "neutral",
        Family {
            hue: 231.49,
            min: 0.02,
            max: 0.08,
            slot: 8,
        },
    ),
    (
        "blue",
        Family {
            hue: 231.49,
            min: 0.1,
            max: 0.68,
            slot: 4,
        },
    ),
    (
        "green",
        Family {
            hue: 158.68,
            min: 0.1,
            max: 0.76,
            slot: 2,
        },
    ),
    (
        "red",
        Family {
            hue: 20.0,
            min: 0.1,
            max: 0.92,
            slot: 1,
        },
    ),
    (
        "yellow",
        Family {
            hue: 82.36,
            min: 0.5,
            max: 1.0,
            slot: 3,
        },
    ),
    (
        "orange",
        Family {
            hue: 52.0,
            min: 0.12,
            max: 0.85,
            slot: 3,
        },
    ),
    (
        "violet",
        Family {
            hue: 295.0,
            min: 0.2,
            max: 0.6,
            slot: 5,
        },
    ),
    (
        "calamine",
        Family {
            hue: 202.43,
            min: 0.1,
            max: 0.74,
            slot: 6,
        },
    ),
    (
        "thinkingSlate",
        Family {
            hue: 231.49,
            min: 0.08,
            max: 0.2,
            slot: 4,
        },
    ),
    (
        "thinkingBlue",
        Family {
            hue: 231.49,
            min: 0.2,
            max: 0.45,
            slot: 4,
        },
    ),
    (
        "thinkingPeriwinkle",
        Family {
            hue: 263.25,
            min: 0.3,
            max: 0.6,
            slot: 6,
        },
    ),
    (
        "thinkingViolet",
        Family {
            hue: 295.0,
            min: 0.4,
            max: 0.75,
            slot: 5,
        },
    ),
    (
        "thinkingMagenta",
        Family {
            hue: 337.5,
            min: 0.5,
            max: 0.85,
            slot: 13,
        },
    ),
    (
        "thinkingRed",
        Family {
            hue: 20.0,
            min: 0.95,
            max: 1.0,
            slot: 1,
        },
    ),
];

fn family(name: &str) -> Family {
    FAMILIES
        .iter()
        .find(|entry| entry.0 == name)
        .map(|entry| entry.1)
        .expect("every family is in FAMILIES")
}

// `TOKEN_FAMILIES` (`system-theme.ts:66-133`), in pi's declaration order.
const TOKEN_FAMILIES: [(&str, &str); 56] = [
    ("selectedBg", "blue"),
    ("searchMatchBg", "orange"),
    ("userMessageBg", "blue"),
    ("customMessageBg", "violet"),
    ("toolPendingBg", "neutral"),
    ("toolSuccessBg", "green"),
    ("toolErrorBg", "red"),
    ("text", "neutral"),
    ("userMessageText", "neutral"),
    ("customMessageText", "neutral"),
    ("toolTitle", "neutral"),
    ("syntaxOperator", "neutral"),
    ("syntaxPunctuation", "neutral"),
    ("muted", "neutral"),
    ("dim", "neutral"),
    ("thinkingText", "neutral"),
    ("toolOutput", "neutral"),
    ("mdLinkUrl", "neutral"),
    ("mdQuote", "neutral"),
    ("mdQuoteBorder", "neutral"),
    ("mdHr", "neutral"),
    ("mdCodeBlockBorder", "neutral"),
    ("toolDiffContext", "neutral"),
    ("syntaxComment", "neutral"),
    ("scrollbarTrack", "neutral"),
    ("scrollbarThumb", "neutral"),
    ("searchMatchText", "neutral"),
    ("borderMuted", "neutral"),
    ("accent", "violet"),
    ("borderAccent", "violet"),
    ("customMessageLabel", "violet"),
    ("mdCode", "violet"),
    ("mdListBullet", "violet"),
    ("syntaxType", "violet"),
    ("border", "blue"),
    ("mdLink", "blue"),
    ("syntaxKeyword", "blue"),
    ("syntaxVariable", "calamine"),
    ("success", "green"),
    ("mdCodeBlock", "green"),
    ("toolDiffAdded", "green"),
    ("bashMode", "green"),
    ("syntaxNumber", "green"),
    ("error", "red"),
    ("toolDiffRemoved", "red"),
    ("warning", "yellow"),
    ("mdHeading", "yellow"),
    ("syntaxFunction", "yellow"),
    ("syntaxString", "orange"),
    ("thinkingOff", "neutral"),
    ("thinkingMinimal", "thinkingSlate"),
    ("thinkingLow", "thinkingBlue"),
    ("thinkingMedium", "thinkingPeriwinkle"),
    ("thinkingHigh", "thinkingViolet"),
    ("thinkingXhigh", "thinkingMagenta"),
    ("thinkingMax", "thinkingRed"),
];

// `TOKEN_SLOTS`: palette slots for tokens that would otherwise share a hue
// with a similar token (`system-theme.ts:135`).
const TOKEN_SLOTS: [(&str, u8); 3] = [
    ("syntaxString", 2),
    ("syntaxNumber", 5),
    ("searchMatchBg", 3),
];

fn family_of(token: &str) -> Family {
    family(
        TOKEN_FAMILIES
            .iter()
            .find(|entry| entry.0 == token)
            .map(|entry| entry.1)
            .expect("every token is in TOKEN_FAMILIES"),
    )
}

fn slot_of(token: &str) -> u8 {
    TOKEN_SLOTS
        .iter()
        .find(|entry| entry.0 == token)
        .map_or_else(|| family_of(token).slot, |entry| entry.1)
}

// ============================================================================
// Tokens and surfaces
// ============================================================================

// Either kind of theme token, so the rules read like pi's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Token {
    Fg(ThemeColor),
    Bg(ThemeBg),
}

impl Token {
    fn name(self) -> &'static str {
        match self {
            Token::Fg(token) => token.as_str(),
            Token::Bg(token) => token.as_str(),
        }
    }

    fn from_name(name: &str) -> Option<Token> {
        if let Some(token) = ThemeBg::from_name(name) {
            return Some(Token::Bg(token));
        }
        ThemeColor::from_name(name).map(Token::Fg)
    }
}

const fn fg(token: ThemeColor) -> Token {
    Token::Fg(token)
}

const fn bg(token: ThemeBg) -> Token {
    Token::Bg(token)
}

// A surface a token is drawn on: the background itself, the panels, or the
// scrollbar track (`system-theme.ts:244-246`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Surface {
    Background,
    ScrollbarTrack,
    Panel(ThemeBg),
}

impl Surface {
    // pi keys its solved map by the token name, so `background` and
    // `scrollbarTrack` share one namespace with the tokens.
    fn key(self) -> &'static str {
        match self {
            Surface::Background => "background",
            Surface::ScrollbarTrack => "scrollbarTrack",
            Surface::Panel(token) => token.as_str(),
        }
    }
}

// ============================================================================
// Contrast levels and rules
// ============================================================================

// A target-lightness curve: a polynomial in the surface's OKLab lightness
// giving the OKLab lightness a token needs on it, plus the range of surface
// lightness where the level can be reached (`system-theme.ts:143-148`).
#[derive(Debug, Clone, Copy)]
struct Curve {
    coefficients: [f64; 6],
    reachable: (f64, f64),
}

// `LEVELS` (`system-theme.ts:150-241`), as `(level, dark, light)`.
const LEVELS: [(Level, Curve, Curve); 13] = [
    (
        Level::Panel,
        Curve {
            coefficients: [0.29131, -0.39746, 2.33185, -0.85524, -1.2076, 0.86276],
            reachable: (0.0, 0.979),
        },
        Curve {
            coefficients: [-3.74073, 27.94549, -78.44258, 112.6798, -79.60015, 22.11277],
            reachable: (0.348, 1.0),
        },
    ),
    (
        Level::Track,
        Curve {
            coefficients: [0.39028, -0.23015, 0.83573, 2.43829, -4.38292, 2.01582],
            reachable: (0.0, 0.946),
        },
        Curve {
            coefficients: [
                -5.24921, 38.37322, -107.28833, 152.10005, -106.17127, 29.18061,
            ],
            reachable: (0.368, 1.0),
        },
    ),
    (
        Level::Thinking0,
        Curve {
            coefficients: [0.52988, -0.05809, -0.30924, 4.63567, -6.52933, 2.89108],
            reachable: (0.0, 0.873),
        },
        Curve {
            coefficients: [
                -28.27749, 182.85284, -469.62416, 603.15916, -384.59976, 97.35147,
            ],
            reachable: (0.51, 1.0),
        },
    ),
    (
        Level::Thinking1,
        Curve {
            coefficients: [0.55278, -0.03667, -0.45659, 4.95347, -6.90265, 3.0706],
            reachable: (0.0, 0.858),
        },
        Curve {
            coefficients: [
                -37.10484, 235.86282, -596.62344, 754.3633, -474.00763, 118.3551,
            ],
            reachable: (0.535, 1.0),
        },
    ),
    (
        Level::Thinking2,
        Curve {
            coefficients: [0.57486, -0.01765, -0.58987, 5.25227, -7.27175, 3.25532],
            reachable: (0.0, 0.842),
        },
        Curve {
            coefficients: [
                -59.89653, 377.05024, -945.07843, 1182.03145, -734.96375, 181.68658,
            ],
            reachable: (0.556, 1.0),
        },
    ),
    (
        Level::Thinking3,
        Curve {
            coefficients: [0.59621, -0.00062, -0.71148, 5.53588, -7.6392, 3.44606],
            reachable: (0.0, 0.827),
        },
        Curve {
            coefficients: [
                -72.07122,
                445.84082,
                -1099.57352,
                1353.88793,
                -829.53392,
                202.26164,
            ],
            reachable: (0.58, 1.0),
        },
    ),
    (
        Level::Thinking4,
        Curve {
            coefficients: [0.61691, 0.01462, -0.82288, 5.80651, -8.00641, 3.64333],
            reachable: (0.0, 0.811),
        },
        Curve {
            coefficients: [
                -110.14338,
                674.21488,
                -1645.75941,
                2004.32367,
                -1215.15899,
                293.3183,
            ],
            reachable: (0.6, 1.0),
        },
    ),
    (
        Level::Thinking5,
        Curve {
            coefficients: [0.63702, 0.02826, -0.92498, 6.06465, -8.37246, 3.84651],
            reachable: (0.0, 0.795),
        },
        Curve {
            coefficients: [
                -175.47701,
                1063.54495,
                -2570.70594,
                3098.80776,
                -1860.15527,
                444.76392,
            ],
            reachable: (0.62, 1.0),
        },
    ),
    (
        Level::Thinking6,
        Curve {
            coefficients: [0.65658, 0.04044, -1.01835, 6.30989, -8.73529, 4.05439],
            reachable: (0.0, 0.779),
        },
        Curve {
            coefficients: [
                -183.81712,
                1094.70055,
                -2602.68539,
                3088.71276,
                -1826.91131,
                430.75931,
            ],
            reachable: (0.643, 1.0),
        },
    ),
    (
        Level::Subtle,
        Curve {
            coefficients: [0.56762, -0.02475, -0.5383, 5.12628, -7.10931, 3.17324],
            reachable: (0.0, 0.848),
        },
        Curve {
            coefficients: [
                -232.85459,
                1376.54473,
                -3249.11801,
                3827.91186,
                -2248.29472,
                526.55751,
            ],
            reachable: (0.657, 1.0),
        },
    ),
    (
        Level::Thumb,
        Curve {
            coefficients: [0.60323, 0.00278, -0.73328, 5.57157, -7.68067, 3.46933],
            reachable: (0.0, 0.823),
        },
        Curve {
            coefficients: [
                -82.89897,
                511.01355,
                -1255.98095,
                1540.76821,
                -940.68087,
                228.58523,
            ],
            reachable: (0.586, 1.0),
        },
    ),
    (
        Level::Readable,
        Curve {
            coefficients: [0.66937, 0.04704, -1.06871, 6.43941, -8.9332, 4.17229],
            reachable: (0.0, 0.77),
        },
        Curve {
            coefficients: [
                -1554.52576,
                8733.56817,
                -19604.93507,
                21977.72696,
                -12300.99599,
                2749.81288,
            ],
            reachable: (0.751, 1.0),
        },
    ),
    (
        Level::Emphasis,
        Curve {
            coefficients: [0.7303, 0.07695, -1.31626, 7.1681, -10.14436, 4.92846],
            reachable: (0.0, 0.712),
        },
        Curve {
            coefficients: [
                -4948.31942,
                26870.91986,
                -58334.48399,
                63280.17197,
                -34298.01053,
                7430.30146,
            ],
            reachable: (0.811, 1.0),
        },
    ),
];

// `textOnPanel` and `text` live in their own table below because pi spells
// them out last (`system-theme.ts:237-241`).
const LEVELS_TEXT: [(Level, Curve, Curve); 2] = [
    (
        Level::TextOnPanel,
        Curve {
            coefficients: [0.86713, 0.05232, -0.89428, 4.79014, -5.5432, 1.75023],
            reachable: (0.0, 0.542),
        },
        Curve {
            coefficients: [
                -8570.89457,
                43954.60805,
                -90084.00702,
                92220.6791,
                -47152.15802,
                9632.27113,
            ],
            reachable: (0.867, 1.0),
        },
    ),
    (
        Level::Text,
        Curve {
            coefficients: [0.89242, 0.02311, -0.44862, 2.34417, -0.06084, -2.63844],
            reachable: (0.0, 0.5),
        },
        Curve {
            coefficients: [
                -2004.67048,
                6664.47299,
                -6060.70202,
                -1792.61209,
                5133.82359,
                -1939.85583,
            ],
            reachable: (0.894, 1.0),
        },
    ),
];

// A contrast level (`Level`, `system-theme.ts:243`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Panel,
    Track,
    Thinking0,
    Thinking1,
    Thinking2,
    Thinking3,
    Thinking4,
    Thinking5,
    Thinking6,
    Subtle,
    Thumb,
    Readable,
    Emphasis,
    TextOnPanel,
    Text,
}

// The target lightness for a level on a surface, or `None` where the level
// cannot be reached (`levelTarget`, `system-theme.ts:451-456`).
fn level_target(level: Level, appearance: ThemeAppearance, surface_l: f64) -> Option<f64> {
    let curve = LEVELS
        .iter()
        .chain(LEVELS_TEXT.iter())
        .find(|entry| entry.0 == level)
        .map(|entry| match appearance {
            ThemeAppearance::Dark => entry.1,
            ThemeAppearance::Light => entry.2,
        })
        .expect("every level is in LEVELS");
    if surface_l < curve.reachable.0 || surface_l > curve.reachable.1 {
        return None;
    }
    Some(
        curve
            .coefficients
            .iter()
            .enumerate()
            .map(|(power, coefficient)| coefficient * surface_l.powi(power as i32))
            .sum(),
    )
}

// A rule: which level a token needs, and on which surfaces (`system-theme.ts:248-251`).
#[derive(Debug, Clone, Copy)]
struct Rule {
    token: Token,
    on: &'static [Surface],
    level: Level,
}

// `TOOL_PANELS` / `MESSAGE_PANELS` / `PANELS` (`system-theme.ts:257-273`).
const TOOL_PANELS: &[Surface] = &[
    Surface::Panel(ThemeBg::ToolPendingBg),
    Surface::Panel(ThemeBg::ToolSuccessBg),
    Surface::Panel(ThemeBg::ToolErrorBg),
];
const PANELS: &[ThemeBg] = &[
    ThemeBg::UserMessageBg,
    ThemeBg::ToolPendingBg,
    ThemeBg::ToolSuccessBg,
    ThemeBg::ToolErrorBg,
    ThemeBg::SelectedBg,
    ThemeBg::SearchMatchBg,
    ThemeBg::CustomMessageBg,
];

// pi's `PANELS.includes(token)` (`:527`): a string test, so a foreground token
// never matches even though the Rust enum is a different type.
fn is_panel(name: &str) -> bool {
    PANELS.iter().any(|panel| panel.as_str() == name)
}

const BACKGROUND: &[Surface] = &[Surface::Background];
// `["background", "selectedBg", ...TOOL_PANELS]` (`:296`).
const READABLE_SURFACES: &[Surface] = &[
    Surface::Background,
    Surface::Panel(ThemeBg::SelectedBg),
    Surface::Panel(ThemeBg::ToolPendingBg),
    Surface::Panel(ThemeBg::ToolSuccessBg),
    Surface::Panel(ThemeBg::ToolErrorBg),
];
// `["background", "selectedBg", "customMessageBg", ...TOOL_PANELS]` (`:297-298`).
const SUBTLE_SURFACES: &[Surface] = &[
    Surface::Background,
    Surface::Panel(ThemeBg::SelectedBg),
    Surface::Panel(ThemeBg::CustomMessageBg),
    Surface::Panel(ThemeBg::ToolPendingBg),
    Surface::Panel(ThemeBg::ToolSuccessBg),
    Surface::Panel(ThemeBg::ToolErrorBg),
];
// `["background", "customMessageBg", ...TOOL_PANELS]` (`:304`).
const CUSTOM_SURFACES: &[Surface] = &[
    Surface::Background,
    Surface::Panel(ThemeBg::CustomMessageBg),
    Surface::Panel(ThemeBg::ToolPendingBg),
    Surface::Panel(ThemeBg::ToolSuccessBg),
    Surface::Panel(ThemeBg::ToolErrorBg),
];
// `["background", ...TOOL_PANELS]` (`:307`, `:312`, `:313`).
const TOOL_SURFACES: &[Surface] = &[
    Surface::Background,
    Surface::Panel(ThemeBg::ToolPendingBg),
    Surface::Panel(ThemeBg::ToolSuccessBg),
    Surface::Panel(ThemeBg::ToolErrorBg),
];
// `["background", ...MESSAGE_PANELS]` (`:310`).
const MESSAGE_SURFACES: &[Surface] = &[
    Surface::Background,
    Surface::Panel(ThemeBg::UserMessageBg),
    Surface::Panel(ThemeBg::CustomMessageBg),
];
// `["background", ...MESSAGE_PANELS, ...TOOL_PANELS]` (`:311`, `:315`).
const MESSAGE_AND_TOOL_SURFACES: &[Surface] = &[
    Surface::Background,
    Surface::Panel(ThemeBg::UserMessageBg),
    Surface::Panel(ThemeBg::CustomMessageBg),
    Surface::Panel(ThemeBg::ToolPendingBg),
    Surface::Panel(ThemeBg::ToolSuccessBg),
    Surface::Panel(ThemeBg::ToolErrorBg),
];

// `READABLE_FLOOR` (`system-theme.ts:347`).
fn readable_floor(appearance: ThemeAppearance) -> Level {
    match appearance {
        ThemeAppearance::Dark => Level::Readable,
        ThemeAppearance::Light => Level::Subtle,
    }
}

// `FOREGROUND_LEVEL` (`system-theme.ts:350`).
const FOREGROUND_LEVEL: Level = Level::Emphasis;

// `FOREGROUND_TOKENS` (`system-theme.ts:353`).
const FOREGROUND_TOKENS: [ThemeColor; 3] = [
    ThemeColor::Text,
    ThemeColor::UserMessageText,
    ThemeColor::ToolTitle,
];

fn each(tokens: &[Token], on: &'static [Surface], level: Level) -> Vec<Rule> {
    tokens
        .iter()
        .map(|token| Rule {
            token: *token,
            on,
            level,
        })
        .collect()
}

fn token(name: &str) -> Token {
    Token::from_name(name).expect("every rule names a real token")
}

// `RULES` (`system-theme.ts:289-345`).
// Upstream rule-table parity: it is one flat list of rules in `system-theme.ts`
// and splitting it would obscure the 1:1 mapping.
#[allow(clippy::too_many_lines)]
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let panel_tokens: Vec<Token> = PANELS.iter().copied().map(bg).collect();
        let mut rules = each(&panel_tokens, BACKGROUND, Level::Panel);
        rules.push(Rule {
            token: token("text"),
            on: BACKGROUND,
            level: Level::Text,
        });
        rules.push(Rule {
            token: token("text"),
            on: &[Surface::Panel(ThemeBg::SelectedBg)],
            level: Level::TextOnPanel,
        });
        rules.push(Rule {
            token: token("userMessageText"),
            on: &[Surface::Panel(ThemeBg::UserMessageBg)],
            level: Level::TextOnPanel,
        });
        rules.push(Rule {
            token: token("toolTitle"),
            on: TOOL_PANELS,
            level: Level::TextOnPanel,
        });
        rules.extend(each(
            &[
                token("accent"),
                token("success"),
                token("error"),
                token("warning"),
            ],
            READABLE_SURFACES,
            Level::Readable,
        ));
        rules.push(Rule {
            token: token("muted"),
            on: SUBTLE_SURFACES,
            level: Level::Readable,
        });
        rules.push(Rule {
            token: token("dim"),
            on: SUBTLE_SURFACES,
            level: Level::Subtle,
        });
        rules.push(Rule {
            token: token("thinkingText"),
            on: BACKGROUND,
            level: Level::Readable,
        });
        rules.push(Rule {
            token: token("customMessageText"),
            on: CUSTOM_SURFACES,
            level: Level::Readable,
        });
        rules.push(Rule {
            token: token("customMessageLabel"),
            on: SUBTLE_SURFACES,
            level: Level::Readable,
        });
        rules.push(Rule {
            token: token("toolOutput"),
            on: TOOL_SURFACES,
            level: Level::Readable,
        });
        rules.extend(each(
            &[
                token("mdHeading"),
                token("mdLink"),
                token("mdLinkUrl"),
                token("mdCode"),
                token("mdQuote"),
                token("mdCodeBlockBorder"),
                token("mdListBullet"),
            ],
            MESSAGE_SURFACES,
            Level::Readable,
        ));
        rules.push(Rule {
            token: token("mdCodeBlock"),
            on: MESSAGE_AND_TOOL_SURFACES,
            level: Level::Readable,
        });
        rules.extend(each(
            &[
                token("toolDiffAdded"),
                token("toolDiffRemoved"),
                token("toolDiffContext"),
            ],
            TOOL_SURFACES,
            Level::Readable,
        ));
        rules.extend(each(
            &[
                token("syntaxComment"),
                token("syntaxKeyword"),
                token("syntaxFunction"),
                token("syntaxVariable"),
                token("syntaxString"),
                token("syntaxNumber"),
                token("syntaxType"),
                token("syntaxOperator"),
                token("syntaxPunctuation"),
            ],
            MESSAGE_AND_TOOL_SURFACES,
            Level::Readable,
        ));
        rules.push(Rule {
            token: token("searchMatchText"),
            on: &[Surface::Panel(ThemeBg::SearchMatchBg)],
            level: Level::Readable,
        });
        rules.extend(each(
            &[token("bashMode"), token("border"), token("borderAccent")],
            BACKGROUND,
            Level::Readable,
        ));
        rules.push(Rule {
            token: token("borderMuted"),
            on: BACKGROUND,
            level: Level::Subtle,
        });
        rules.extend(each(
            &[token("mdQuoteBorder"), token("mdHr")],
            MESSAGE_AND_TOOL_SURFACES,
            Level::Readable,
        ));
        rules.push(Rule {
            token: token("scrollbarTrack"),
            on: BACKGROUND,
            level: Level::Track,
        });
        rules.push(Rule {
            token: token("scrollbarThumb"),
            on: &[Surface::ScrollbarTrack],
            level: Level::Thumb,
        });
        rules.extend(
            [
                ("thinkingOff", Level::Thinking0),
                ("thinkingMinimal", Level::Thinking1),
                ("thinkingLow", Level::Thinking2),
                ("thinkingMedium", Level::Thinking3),
                ("thinkingHigh", Level::Thinking4),
                ("thinkingXhigh", Level::Thinking5),
                ("thinkingMax", Level::Thinking6),
            ]
            .map(|(name, level)| Rule {
                token: token(name),
                on: BACKGROUND,
                level,
            }),
        );
        rules
    })
}

// `SOLVE_ORDER`: tokens in dependency order, every surface before the tokens
// drawn on it (`system-theme.ts:363-377`).
fn solve_order() -> &'static [Token] {
    static ORDER: OnceLock<Vec<Token>> = OnceLock::new();
    ORDER.get_or_init(|| {
        fn surface_token(surface: Surface) -> Option<Token> {
            match surface {
                Surface::Background => None,
                Surface::Panel(panel) => Some(Token::Bg(panel)),
                Surface::ScrollbarTrack => Some(token("scrollbarTrack")),
            }
        }
        fn visit(current: Token, rules: &[Rule], order: &mut Vec<Token>) {
            if order.contains(&current) {
                return;
            }
            for rule in rules.iter().filter(|rule| rule.token == current) {
                for surface in rule.on {
                    if let Some(dependency) = surface_token(*surface) {
                        visit(dependency, rules, order);
                    }
                }
            }
            order.push(current);
        }
        let rules = rules();
        let mut order: Vec<Token> = Vec::with_capacity(TOKEN_FAMILIES.len());
        for rule in rules {
            visit(rule.token, rules, &mut order);
        }
        order
    })
}

// ============================================================================
// Public API
// ============================================================================

/// What the generator knows about the terminal (`system-theme.ts:381-390`).
///
/// The reported colors, plus the two knobs pi derives from its state: the
/// saturation multiplier (0 grayscales the first frame) and an appearance
/// hint for terminals that reported no background of their own.
// `SystemThemeInput` (`system-theme.ts:381-390`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemThemeInput {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    // ANSI colors 0-15; used only when all sixteen arrived.
    pub palette: Option<Vec<Rgb>>,
    // Saturation multiplier from 0 (grayscale) to 1. The first frame renders
    // in grayscale until colors arrive.
    pub saturation: Option<f64>,
    // Appearance when the terminal did not report its background, e.g. from
    // its light/dark report or `COLORFGBG`.
    pub appearance_hint: Option<ThemeAppearance>,
}

impl SystemThemeInput {
    /// Exactly what the terminal reported, at full saturation.
    // The terminal's reported colors, with full saturation.
    pub fn from_terminal(colors: &TerminalColors) -> SystemThemeInput {
        SystemThemeInput {
            foreground: colors.foreground,
            background: colors.background,
            palette: colors.palette.clone(),
            saturation: Some(1.0),
            appearance_hint: None,
        }
    }
}

/// The generated theme: one [`ColorValue`] per token (`system-theme.ts:392-398`).
///
/// Values are hex strings, palette indices, or [`ColorValue::Default`] to keep
/// the terminal's own color; `dim` names the foreground tokens a terminal that
/// reported nothing renders faint (SGR 2).
// `SystemThemeColors` (`system-theme.ts:392-398`). Colors are hex strings,
// ANSI palette indices, or [`ColorValue::Default`] for the terminal default.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemThemeColors {
    pub colors: BTreeMap<ThemeColor, ColorValue>,
    pub backgrounds: BTreeMap<ThemeBg, ColorValue>,
    // Foreground tokens rendered faint (SGR 2), for terminals that did not
    // report colors.
    pub dim: Vec<ThemeColor>,
    pub appearance: Option<ThemeAppearance>,
}

impl SystemThemeColors {
    fn new() -> SystemThemeColors {
        SystemThemeColors {
            colors: BTreeMap::new(),
            backgrounds: BTreeMap::new(),
            dim: Vec::new(),
            appearance: None,
        }
    }

    fn set(&mut self, name: &str, value: ColorValue) {
        match Token::from_name(name) {
            Some(Token::Fg(token)) => {
                self.colors.insert(token, value);
            }
            Some(Token::Bg(token)) => {
                self.backgrounds.insert(token, value);
            }
            None => {}
        }
    }

    /// The foreground token's value, or [`ColorValue::Default`] when unset.
    // A foreground token's colour, defaulting to the terminal default like
    // pi's `Record` lookup would.
    pub fn color(&self, token: ThemeColor) -> ColorValue {
        self.colors
            .get(&token)
            .cloned()
            .unwrap_or(ColorValue::Default)
    }

    /// The background token's value, or [`ColorValue::Default`] when unset.
    // A background token's colour.
    pub fn background(&self, token: ThemeBg) -> ColorValue {
        self.backgrounds
            .get(&token)
            .cloned()
            .unwrap_or(ColorValue::Default)
    }
}

// ============================================================================
// Generation
// ============================================================================

fn clamp(value: f64, min: f64, max: f64) -> f64 {
    max.min(min.max(value))
}

fn hex_of(color: Rgb) -> ColorValue {
    ColorValue::Text(color_to_hex(rgb_color(color.r, color.g, color.b)))
}

// Saturation weight at a lightness: a Gaussian (center 0.5, sigma 0.25), 0 at
// black and white, 1 in the middle (`bellWeight`, `system-theme.ts:436-439`).
fn bell_weight(lightness: f64) -> f64 {
    fn gaussian(x: f64) -> f64 {
        (-((x - 0.5).powi(2)) / (2.0 * 0.25f64.powi(2))).exp()
    }
    (gaussian(lightness) - gaussian(0.0)) / (1.0 - gaussian(0.0))
}

// A family's saturation curve relative to its maximum: 1 at mid lightness,
// `min / max` at black and white (`saturationCurve`, `system-theme.ts:441-444`).
fn saturation_curve(family: &Family, lightness: f64) -> f64 {
    let floor = if family.max > 0.0 {
        family.min / family.max
    } else {
        1.0
    };
    floor + (1.0 - floor) * bell_weight(lightness)
}

// A terminal color's OKHSL channels and its OKLCH chroma
// (`SourceColor`, `system-theme.ts:601-607`).
#[derive(Debug, Clone, Copy)]
struct SourceColor {
    h: f64,
    s: f64,
    l: f64,
    chroma: f64,
}

fn source_of(color: Rgb) -> SourceColor {
    let okhsl = rgb_to_okhsl(color);
    SourceColor {
        h: okhsl.h,
        s: okhsl.s,
        l: okhsl.l,
        chroma: color_to_oklch(crate::color::rgb_color(color.r, color.g, color.b)).c,
    }
}

// A source color's hue at another OKHSL lightness (`anchored`,
// `system-theme.ts:609-622`). Its saturation applies at its own lightness and
// falls off toward black and white along the family's saturation curve, never
// rising above it. Chroma is capped the same way, so a pastel palette color
// never gains chroma at another lightness.
fn anchored(source: SourceColor, family: &Family, lightness: f64, saturation: f64) -> Rgb {
    let anchor = saturation_curve(family, source.l);
    let falloff = if anchor > 0.0 {
        1.0_f64.min(saturation_curve(family, lightness) / anchor)
    } else {
        1.0
    };
    let color = okhsl_to_rgb(source.h, source.s * falloff * saturation, lightness);
    let cap = source.chroma * falloff * saturation;
    let channels = color_to_oklch(crate::color::rgb_color(color.r, color.g, color.b));
    if channels.c <= cap {
        return color;
    }
    color_to_rgb(oklch_color(channels.l, cap, source.h))
}

// Move a text color toward white or black until it reaches the WCAG minimum
// on every surface (`withTextContrast`, `system-theme.ts:624-640`).
fn with_text_contrast(color: Rgb, surfaces: &[Rgb], lighter: bool) -> Rgb {
    let meets = |candidate: Rgb| {
        surfaces
            .iter()
            .all(|surface| wcag_contrast(candidate, *surface) >= TEXT_MINIMUM_WCAG_CONTRAST)
    };
    if meets(color) {
        return color;
    }
    let okhsl = rgb_to_okhsl(color);
    let at = |lightness: f64| okhsl_to_rgb(okhsl.h, okhsl.s, lightness);
    let extreme = if lighter { 1.0 } else { 0.0 };
    if !meets(at(extreme)) {
        return at(extreme);
    }
    let mut low = okhsl.l;
    let mut high = extreme;
    for _ in 0..20 {
        let middle = low.midpoint(high);
        if meets(at(middle)) {
            high = middle;
        } else {
            low = middle;
        }
    }
    at(high)
}

/// Generate the `system` theme's color values from the terminal's report.
///
/// Every token must reach its contrast level on each surface it is drawn on;
/// when a mid-gray background cannot satisfy them all, the levels relax as
/// little as a binary search allows (`system-theme.ts:461-573`). With no
/// reported background the indexed fallback applies: tokens become ANSI
/// palette indices or [`ColorValue::Default`], which the terminal renders
/// with its own theme.
// The generator (`generateSystemThemeColors`, `system-theme.ts:461-573`).
// Upstream parity: this mirrors `generateSystemThemeColors` step for step and
// splitting it would obscure that mapping.
#[allow(clippy::too_many_lines)]
pub fn generate_system_theme_colors(input: &SystemThemeInput) -> SystemThemeColors {
    let saturation = clamp(input.saturation.unwrap_or(1.0), 0.0, 1.0);
    let Some(background) = input.background else {
        return indexed_colors(saturation, input.appearance_hint);
    };
    let palette: Option<Vec<SourceColor>> = input
        .palette
        .as_ref()
        .filter(|palette| palette.len() == 16)
        .map(|palette| palette.iter().copied().map(source_of).collect());

    let appearance = terminal_appearance(background, input.foreground);
    let lighter = appearance == ThemeAppearance::Dark;
    let extreme = if lighter { 1.0 } else { 0.0 };
    let background_l = oklab_lightness(background);

    // A token's color at an OKLab lightness (`paint`, `system-theme.ts:474-484`).
    let paint = |name: &str, oklab_l: f64| -> Rgb {
        let lightness = oklab_to_okhsl_lightness(oklab_l);
        let family = family_of(name);
        match &palette {
            None => okhsl_to_rgb(
                family.hue,
                (family.min + (family.max - family.min) * bell_weight(lightness)) * saturation,
                lightness,
            ),
            Some(palette) => anchored(
                palette[slot_of(name) as usize],
                &family,
                lightness,
                saturation,
            ),
        }
    };

    // The lightness a rule needs on a surface, relaxed by `t` (`target`,
    // `system-theme.ts:486-499`).
    let target = |level: Level, surface_l: f64, t: f64| -> Option<f64> {
        let reached = level_target(level, appearance, surface_l);
        if reached.is_none() && t == 0.0 {
            return None;
        }
        let distance = reached.unwrap_or(extreme) - surface_l;
        let floor = level_target(readable_floor(appearance), appearance, surface_l)
            .unwrap_or(extreme)
            - surface_l;
        let compressed = if distance.abs() > floor.abs() {
            distance - (distance - floor) * t.min(1.0)
        } else {
            distance
        };
        Some(surface_l + compressed * (1.0 - (t - 1.0).max(0.0)))
    };

    // Keep a panel light enough (or dark enough) that white (or black) text
    // still reaches the body text minimum on it (`limitPanel`,
    // `system-theme.ts:501-517`).
    let extreme_text = if lighter {
        Rgb {
            r: 255.0,
            g: 255.0,
            b: 255.0,
        }
    } else {
        Rgb {
            r: 0.0,
            g: 0.0,
            b: 0.0,
        }
    };
    let limit_panel = |name: &str, l: f64| -> Rgb {
        let readable =
            |color: Rgb| wcag_contrast(extreme_text, color) >= TEXT_MINIMUM_WCAG_CONTRAST;
        let color = paint(name, l);
        if readable(color) {
            return color;
        }
        let mut low = background_l;
        let mut high = l;
        for _ in 0..20 {
            let middle = low.midpoint(high);
            if readable(paint(name, middle)) {
                low = middle;
            } else {
                high = middle;
            }
        }
        paint(name, low)
    };

    let rules = rules();
    let solve = |t: f64| -> Option<BTreeMap<&'static str, Rgb>> {
        let mut colors: BTreeMap<&'static str, Rgb> = BTreeMap::new();
        colors.insert("background", background);
        for entry in solve_order() {
            let mut targets: Vec<f64> = Vec::new();
            for rule in rules.iter().filter(|rule| rule.token == *entry) {
                for surface in rule.on {
                    let surface_l =
                        oklab_lightness(colors.get(surface.key()).copied().unwrap_or(background));
                    let value = target(rule.level, surface_l, t)?;
                    if !(0.0..=1.0).contains(&value) {
                        return None;
                    }
                    targets.push(value);
                }
            }
            let l = if lighter {
                targets.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            } else {
                targets.iter().copied().fold(f64::INFINITY, f64::min)
            };
            let name = entry.name();
            let color = if is_panel(name) {
                limit_panel(name, l)
            } else {
                paint(name, l)
            };
            colors.insert(name, color);
        }
        Some(colors)
    };

    let mut relaxation = 0.0;
    let mut colors = solve(0.0);
    if colors.is_none() {
        // Mid-gray backgrounds cannot fit every level: relax as little as
        // possible. Full relaxation always fits.
        let mut low: f64 = 0.0;
        let mut high = 2.0;
        colors = solve(high);
        for _ in 0..20 {
            let middle = low.midpoint(high);
            match solve(middle) {
                Some(attempt) => {
                    high = middle;
                    colors = Some(attempt);
                }
                None => low = middle,
            }
        }
        relaxation = high;
    }
    let solved = colors.unwrap_or_default();

    let surfaces_of = |token: &Token| -> Vec<Rgb> {
        rules
            .iter()
            .filter(|rule| rule.token == *token)
            .flat_map(|rule| {
                rule.on
                    .iter()
                    .map(|surface| solved.get(surface.key()).copied().unwrap_or(background))
            })
            .collect()
    };

    let mut result = SystemThemeColors::new();
    result.appearance = Some(appearance);
    for (name, _) in &TOKEN_FAMILIES {
        result.set(
            name,
            match solved.get(name) {
                Some(color) => hex_of(*color),
                None => ColorValue::Default,
            },
        );
    }

    for token in FOREGROUND_TOKENS {
        let name = token.as_str();
        let surfaces = surfaces_of(&fg(token));
        // Body text uses the terminal's own foreground where it is clearly
        // stronger than muted text; otherwise the foreground's hue at just
        // enough lightness.
        let mut text = solved.get(name).copied();
        if let Some(foreground) = input.foreground {
            let targets: Vec<Option<f64>> = surfaces
                .iter()
                .map(|surface| target(FOREGROUND_LEVEL, oklab_lightness(*surface), relaxation))
                .collect();
            if targets
                .iter()
                .all(|value| matches!(value, Some(value) if (0.0..=1.0).contains(value)))
            {
                let needed = if lighter {
                    targets
                        .iter()
                        .flatten()
                        .copied()
                        .fold(f64::NEG_INFINITY, f64::max)
                } else {
                    targets
                        .iter()
                        .flatten()
                        .copied()
                        .fold(f64::INFINITY, f64::min)
                };
                let foreground_l = oklab_lightness(foreground);
                if if lighter {
                    foreground_l >= needed
                } else {
                    foreground_l <= needed
                } {
                    result.set(name, ColorValue::Default);
                    continue;
                }
                text = Some(anchored(
                    source_of(foreground),
                    &family("neutral"),
                    oklab_to_okhsl_lightness(needed),
                    saturation,
                ));
            }
        }
        // Body text keeps at least 4.5:1 on the surfaces it is drawn on, even
        // on relaxed mid-gray backgrounds.
        if let Some(text) = text {
            result.set(name, hex_of(with_text_contrast(text, &surfaces, lighter)));
        }
    }
    result
}

// Colors for terminals that reported nothing (`indexedColors`,
// `system-theme.ts:642-656`): the terminal renders ANSI indices 0-15 and the
// default colors with its own theme, so they fit any background. Neutral
// tokens below body text are faint (SGR 2) instead of bright black, which some
// themes make nearly invisible. Panels have no background.
fn indexed_colors(saturation: f64, appearance: Option<ThemeAppearance>) -> SystemThemeColors {
    let mut colors = SystemThemeColors::new();
    colors.appearance = appearance;
    for (name, family_name) in &TOKEN_FAMILIES {
        let Some(entry) = Token::from_name(name) else {
            continue;
        };
        if let Token::Bg(_) = entry {
            colors.set(name, ColorValue::Default);
            continue;
        }
        let neutral = *family_name == "neutral";
        colors.set(
            name,
            if !neutral && saturation > 0.0 {
                ColorValue::Index(slot_of(name))
            } else {
                ColorValue::Default
            },
        );
        if neutral
            && let Token::Fg(fg) = entry
            && !FOREGROUND_TOKENS.contains(&fg)
        {
            colors.dim.push(fg);
        }
    }
    colors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_has_a_family_and_a_slot() {
        for (name, _) in &TOKEN_FAMILIES {
            assert!(Token::from_name(name).is_some(), "{name} is not a token");
            assert!(slot_of(name) < 16, "{name} has an out-of-range slot");
        }
        assert_eq!(
            TOKEN_FAMILIES.len(),
            ThemeColor::ALL.len() + ThemeBg::ALL.len()
        );
    }

    #[test]
    fn solve_order_puts_every_surface_first() {
        let order = solve_order();
        assert_eq!(order.len(), TOKEN_FAMILIES.len());
        let position = |name: &str| {
            order
                .iter()
                .position(|entry| entry.name() == name)
                .unwrap_or_else(|| panic!("{name} is missing"))
        };
        assert!(position("selectedBg") < position("text"));
        assert!(position("scrollbarTrack") < position("scrollbarThumb"));
        assert!(position("userMessageBg") < position("userMessageText"));
        assert!(position("searchMatchBg") < position("searchMatchText"));
    }

    #[test]
    fn bell_weight_is_zero_at_black_and_one_in_the_middle() {
        assert!(bell_weight(0.0).abs() < 1e-12);
        assert!((bell_weight(0.5) - 1.0).abs() < 1e-12);
        assert!(bell_weight(1.0) < 0.2);
    }

    #[test]
    fn level_targets_are_reachable_only_inside_their_range() {
        assert_eq!(level_target(Level::Text, ThemeAppearance::Dark, 0.6), None);
        assert!(level_target(Level::Text, ThemeAppearance::Dark, 0.4).is_some());
        assert_eq!(level_target(Level::Text, ThemeAppearance::Light, 0.8), None);
        assert!(level_target(Level::Text, ThemeAppearance::Light, 0.95).is_some());
    }
}
