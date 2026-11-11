//! pi's own pins for the generated `system` theme, translated.
//!
//! Ground truth: `packages/coding-agent/test/system-theme.test.ts` (86 lines).
//! Every value asserted here is that file's; the terminal is injected, never
//! queried, so no test opens `/dev/tty`.

use pi_tui::color::{TerminalColorMode, TerminalColors, color_to_oklch, parse_color, rgb_color};
use pi_tui::oklab::Rgb;
use pi_tui::system_theme::{SystemThemeInput, generate_system_theme_colors};
use pi_tui::theme::{
    ColorValue, SystemThemeMode, ThemeAppearance, ThemeBg, ThemeColor, ThemeExportColors,
    ThemePaths, ThemeRegistry, ThemeStyle, available_themes, generate_system_theme, load_theme,
    oklab_lightness, wcag_contrast,
};

/// Built-in themes are embedded, so only the custom directory needs a path
/// that exists. Nothing is written to it and nothing is read from a real
/// `~/.pi/agent`.
fn theme_paths() -> ThemePaths {
    let dir = std::env::temp_dir().join(format!(
        "pi-tui-system-theme-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    ThemePaths::new(ThemePaths::builtin_dir(), dir)
}

fn rgb(hex: &str) -> Rgb {
    match parse_color(hex).expect("hex") {
        pi_tui::color::Color::Rgb(rgb) => rgb,
        other => panic!("{hex} is not rgb: {other:?}"),
    }
}

fn palette(hexes: &[&str]) -> Vec<Rgb> {
    hexes.iter().copied().map(rgb).collect()
}

fn chroma(color: Rgb) -> f64 {
    color_to_oklch(rgb_color(color.r, color.g, color.b)).c
}

fn hue(color: Rgb) -> f64 {
    color_to_oklch(rgb_color(color.r, color.g, color.b)).h
}

/// `DRACULA`'s 16 ANSI colours (`system-theme.test.ts:21-24`).
const DRACULA_PALETTE: [&str; 16] = [
    "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
    "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
];

/// Catppuccin Frappé's 16 ANSI colours (`system-theme.test.ts:70-73`).
const FRAPPE_PALETTE: [&str; 16] = [
    "#51576d", "#e78284", "#a6d189", "#e5c890", "#8caaee", "#f4b8e4", "#81c8be", "#b5bfe2",
    "#626880", "#e67172", "#8ec772", "#d9ba73", "#7b9ef0", "#f2a4db", "#5abfb5", "#a5adce",
];

/// Dark with a palette, light with an unreadable foreground, background only,
/// and mid-gray (`TERMINALS`, `system-theme.test.ts:20-25`).
fn terminals() -> Vec<(&'static str, SystemThemeInput)> {
    let dracula = SystemThemeInput {
        background: Some(rgb("#282a36")),
        foreground: Some(rgb("#f8f8f2")),
        palette: Some(palette(&DRACULA_PALETTE)),
        ..SystemThemeInput::default()
    };
    vec![
        ("dracula", dracula),
        (
            "solarizedLight",
            SystemThemeInput {
                background: Some(rgb("#fdf6e3")),
                foreground: Some(rgb("#657b83")),
                ..SystemThemeInput::default()
            },
        ),
        (
            "backgroundOnly",
            SystemThemeInput {
                background: Some(rgb("#1e1e1e")),
                ..SystemThemeInput::default()
            },
        ),
        (
            "midGray",
            SystemThemeInput {
                background: Some(rgb("#808080")),
                foreground: Some(rgb("#ffffff")),
                ..SystemThemeInput::default()
            },
        ),
    ]
}

const PANELS: [ThemeBg; 5] = [
    ThemeBg::UserMessageBg,
    ThemeBg::ToolPendingBg,
    ThemeBg::ToolSuccessBg,
    ThemeBg::ToolErrorBg,
    ThemeBg::SelectedBg,
];

/// pi's `resolved()` helper: a `""` panel is the background, a `""` foreground
/// token is the foreground (`system-theme.test.ts:29-32`).
fn hex_of(value: ColorValue) -> String {
    match value {
        ColorValue::Text(text) => text,
        ColorValue::Index(index) => panic!("indexed: {index}"),
        ColorValue::Default => unreachable!(),
    }
}

fn resolved(input: &SystemThemeInput, token: ThemeColor) -> Rgb {
    let value = generate_system_theme_colors(input).color(token);
    if value == ColorValue::Default {
        return input.foreground.expect("terminal default foreground");
    }
    rgb(&hex_of(value))
}

fn resolved_bg(input: &SystemThemeInput, token: ThemeBg) -> Rgb {
    let value = generate_system_theme_colors(input).background(token);
    if value == ColorValue::Default {
        return input.background.expect("terminal default background");
    }
    rgb(&hex_of(value))
}

#[test]
fn body_text_reaches_wcag_on_the_background_and_its_panels() {
    for (name, input) in terminals() {
        let text = resolved(&input, ThemeColor::Text);
        for surface in [
            input.background.expect("background"),
            resolved_bg(&input, ThemeBg::SelectedBg),
        ] {
            assert!(
                wcag_contrast(text, surface) >= 4.5,
                "{name}: {}",
                wcag_contrast(text, surface)
            );
        }
        let title = resolved(&input, ThemeColor::ToolTitle);
        let error_panel = resolved_bg(&input, ThemeBg::ToolErrorBg);
        assert!(
            wcag_contrast(title, error_panel) >= 4.5,
            "{name}: {}",
            wcag_contrast(title, error_panel)
        );
    }
}

#[test]
fn foreground_roles_are_ordered_by_contrast_and_panels_stay_near_the_background() {
    for (name, input) in terminals() {
        let background = input.background.expect("background");
        let background_l = oklab_lightness(background);
        let offset = |token: ThemeColor| oklab_lightness(resolved(&input, token)) - background_l;
        // On mid-gray the levels collapse to the strongest reachable color.
        if name != "midGray" {
            assert!(
                offset(ThemeColor::Text).abs() > offset(ThemeColor::Muted).abs(),
                "{name}"
            );
            assert!(
                offset(ThemeColor::Muted).abs() > offset(ThemeColor::Dim).abs(),
                "{name}"
            );
        }
        let lighter =
            generate_system_theme_colors(&input).appearance == Some(ThemeAppearance::Dark);
        for panel in PANELS {
            let color = resolved_bg(&input, panel);
            let contrast = wcag_contrast(color, background);
            assert!(contrast < 2.0, "{name} {panel:?}: {contrast}");
            assert_eq!(
                oklab_lightness(color) > background_l,
                lighter,
                "{name} {panel:?}"
            );
        }
    }
}

#[test]
fn the_terminal_foreground_and_palette_hues_are_used() {
    let dracula = terminals().remove(0).1;
    // Dracula's own foreground passes the emphasis level, so body text keeps
    // the terminal default.
    assert_eq!(
        generate_system_theme_colors(&dracula).color(ThemeColor::Text),
        ColorValue::Default
    );
    // Solarized's foreground is below 4.5:1 on its own background, so text is
    // darkened instead.
    let solarized = terminals()[1].1.clone();
    assert_ne!(
        generate_system_theme_colors(&solarized).color(ThemeColor::Text),
        ColorValue::Default
    );
    let palette = dracula.palette.clone().expect("palette");
    assert!((hue(resolved(&dracula, ThemeColor::Error)) - hue(palette[1])).abs() < 8.0);
}

#[test]
fn pastel_palette_colors_stay_pastel_at_other_lightnesses() {
    // https://github.com/earendil-works/pi/issues/10255
    let palette = palette(&FRAPPE_PALETTE);
    let frappe = SystemThemeInput {
        background: Some(rgb("#303446")),
        foreground: Some(rgb("#c6d0f5")),
        palette: Some(palette.clone()),
        ..SystemThemeInput::default()
    };
    let pink = palette[5];
    let accent = resolved(&frappe, ThemeColor::Accent);
    // The accent is darker than the pink, but must not gain chroma (it was 2x
    // before the cap).
    assert!(oklab_lightness(accent) < oklab_lightness(pink) - 0.05);
    assert!(chroma(accent) <= chroma(pink) * 1.03);
    for panel in [ThemeBg::UserMessageBg, ThemeBg::CustomMessageBg] {
        assert!(chroma(resolved_bg(&frappe, panel)) <= 0.1, "{panel:?}");
    }
}

#[test]
fn zero_saturation_renders_grayscale() {
    let grayscale = SystemThemeInput {
        saturation: Some(0.0),
        ..terminals().remove(0).1
    };
    let error = generate_system_theme_colors(&grayscale).color(ThemeColor::Error);
    assert!(
        matches!(error, ColorValue::Text(_)),
        "expected a hex colour, got {error:?}"
    );
    assert!(chroma(rgb(&hex_of(error))) < 0.005);
}

#[test]
fn without_a_background_it_falls_back_to_palette_indices_and_faint_text() {
    let generated = generate_system_theme_colors(&SystemThemeInput {
        appearance_hint: Some(ThemeAppearance::Light),
        ..SystemThemeInput::default()
    });
    assert_eq!(generated.appearance, Some(ThemeAppearance::Light));
    assert_eq!(
        (
            generated.color(ThemeColor::Error),
            generated.color(ThemeColor::Text),
            generated.background(ThemeBg::UserMessageBg),
        ),
        (
            ColorValue::Index(1),
            ColorValue::Default,
            ColorValue::Default,
        )
    );
    assert!(generated.dim.contains(&ThemeColor::Muted));
    // Grayscale is the first frame's saturation: every token is the terminal
    // default.
    let pending = generate_system_theme_colors(&SystemThemeInput {
        saturation: Some(0.0),
        ..SystemThemeInput::default()
    });
    assert_eq!(pending.color(ThemeColor::Error), ColorValue::Default);
}

/// pi's `describe("system theme")` block (`system-theme.test.ts:75-86`): the
/// name is listed first, has no export colours, and is generated from the
/// terminal colours.
#[test]
fn the_system_theme_is_listed_first_has_no_export_and_uses_the_terminal_colors() {
    let paths = theme_paths();
    let names: Vec<String> = available_themes(&paths, &ThemeRegistry::new())
        .into_iter()
        .map(|info| info.name)
        .collect();
    assert_eq!(names.first().map(String::as_str), Some("system"));

    let dracula = TerminalColors {
        foreground: Some(rgb("#f8f8f2")),
        background: Some(rgb("#282a36")),
        palette: Some(palette(&DRACULA_PALETTE)),
    };
    let theme = load_theme(
        &paths,
        &ThemeRegistry::new(),
        "system",
        TerminalColorMode::TrueColor,
        &dracula,
    )
    .expect("system");
    assert_eq!(theme.appearance(), ThemeAppearance::Dark);
    assert_eq!(theme.fg_ansi(ThemeColor::Text), "\u{1b}[39m");
    assert!(
        theme.fg_ansi(ThemeColor::Error).starts_with("\u{1b}[38;"),
        "{}",
        theme.fg_ansi(ThemeColor::Error)
    );
    assert_eq!(theme.export_colors(), &ThemeExportColors::default());
}

/// The same block's faint-token pin (`system-theme.test.ts:80-84`). Terminals
/// that report no colours leave the neutral tokens faint.
#[test]
fn faint_tokens_render_with_sgr_two_and_close_it() {
    let theme = generate_system_theme(
        TerminalColorMode::TrueColor,
        &TerminalColors::default(),
        SystemThemeMode::reported(),
    );
    assert_eq!(
        theme.fg(ThemeColor::Muted, "x"),
        "\u{1b}[39m\u{1b}[2mx\u{1b}[22;39m"
    );
    assert_eq!(
        theme.style(
            "x",
            ThemeStyle {
                fg: Some(ThemeColor::Muted),
                ..ThemeStyle::default()
            }
        ),
        "\u{1b}[39m\u{1b}[2mx\u{1b}[22m\u{1b}[39m"
    );
}

/// A new report regenerates the whole theme (`theme-controller.ts:197-211`).
#[test]
fn a_new_report_regenerates_the_theme() {
    let paths = theme_paths();
    let registry = ThemeRegistry::new();
    let dark = TerminalColors {
        foreground: Some(rgb("#f8f8f2")),
        background: Some(rgb("#282a36")),
        palette: None,
    };
    let light = TerminalColors {
        foreground: Some(rgb("#000000")),
        background: Some(rgb("#ffffff")),
        palette: None,
    };
    let first = load_theme(
        &paths,
        &registry,
        "system",
        TerminalColorMode::TrueColor,
        &dark,
    )
    .expect("dark");
    let second = load_theme(
        &paths,
        &registry,
        "system",
        TerminalColorMode::TrueColor,
        &light,
    )
    .expect("light");
    assert_eq!(first.appearance(), ThemeAppearance::Dark);
    assert_eq!(second.appearance(), ThemeAppearance::Light);
    assert_ne!(
        first.fg_ansi(ThemeColor::Border),
        second.fg_ansi(ThemeColor::Border)
    );
}
