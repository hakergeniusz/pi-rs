//! ANSI escape code to HTML converter for the HTML export.
//!
//! Ground truth: `packages/coding-agent/src/core/export-html/ansi-to-html.ts`
//! (the whole file, `:1-297`). Converts terminal ANSI color/style codes to
//! HTML with inline styles:
//!
//! - standard foreground colors (30-37) and bright variants (90-97)
//! - standard background colors (40-47) and bright variants (100-107)
//! - 256-color palette (38;5;N and 48;5;N)
//! - RGB true color (38;2;R;G;B and 48;2;R;G;B)
//! - text styles: bold (1), dim (2), italic (3), underline (4)
//! - reset (0)
//! - OSC 8 hyperlinks with http, https, or mailto URLs; other CSI, OSC, and
//!   APC sequences and stray ESC characters are dropped

use regex::Regex;

/// Standard ANSI color palette 0-15 (`ANSI_COLORS`, `ansi-to-html.ts:16-33`).
const ANSI_COLORS: [&str; 16] = [
    "#000000", // 0: black
    "#800000", // 1: red
    "#008000", // 2: green
    "#808000", // 3: yellow
    "#000080", // 4: blue
    "#800080", // 5: magenta
    "#008080", // 6: cyan
    "#c0c0c0", // 7: white
    "#808080", // 8: bright black
    "#ff0000", // 9: bright red
    "#00ff00", // 10: bright green
    "#ffff00", // 11: bright yellow
    "#0000ff", // 12: bright blue
    "#ff00ff", // 13: bright magenta
    "#00ffff", // 14: bright cyan
    "#ffffff", // 15: bright white
];

/// `ANSI_REGEX` (`ansi-to-html.ts:194-195`): SGR (`ESC[…m`), other CSI
/// sequences, OSC sequences, and APC sequences (terminated by BEL or ST).
/// `\d` is spelled `[0-9]` — JavaScript's `\d` is ASCII-only.
const ANSI_PATTERN: &str = r"\x1b\[([0-9;]*)m|\x1b\[[0-?]*[ -/]*[@-~]|\x1b\]([^\x07\x1b]*)(?:\x07|\x1b\\)|\x1b_[^\x07\x1b]*(?:\x07|\x1b\\)";

fn ansi_regex() -> &'static Regex {
    static REGEX: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    REGEX.get_or_init(|| Regex::new(ANSI_PATTERN).expect("ANSI_REGEX compiles"))
}

/// `OSC8_REGEX` (`ansi-to-html.ts:198`): the OSC 8 hyperlink payload
/// `8;params;url`.
fn osc8_regex() -> &'static Regex {
    static REGEX: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^8;[^;]*;(.*)$").expect("OSC8_REGEX compiles"))
}

/// `SAFE_LINK_REGEX` (`ansi-to-html.ts:200`): only http(s) and mailto links
/// survive into the export.
fn safe_link_regex() -> &'static Regex {
    static REGEX: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"^(?:https?:|mailto:)").expect("SAFE_LINK_REGEX compiles"))
}

/// `escapeHtml` (`ansi-to-html.ts:64-71`).
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#039;")
}

/// `color256ToHex` (`ansi-to-html.ts:38-59`): standard colors, the 6x6x6
/// cube, then the 24-step grayscale ramp.
fn color256_to_hex(index: i64) -> String {
    // Standard colors (0-15)
    if (0..16).contains(&index) {
        return ANSI_COLORS[index as usize].to_string();
    }

    // Color cube (16-231): 6x6x6 = 216 colors
    if (16..232).contains(&index) {
        let cube_index = index - 16;
        let r = cube_index / 36;
        let g = (cube_index % 36) / 6;
        let b = cube_index % 6;
        let to_hex = |n: i64| format!("{:02x}", if n == 0 { 0 } else { 55 + n * 40 });
        return format!("#{}{}{}", to_hex(r), to_hex(g), to_hex(b));
    }

    // Grayscale (232-255): 24 shades. `padStart(2, "0")` is a *minimum*
    // width, so a wider number passes through unchanged — same as Rust's
    // `{:02x}`.
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

/// A parsed SGR state (`TextStyle`, `ansi-to-html.ts:73-80`).
// One flag per text attribute, mirroring upstream's `TextStyle`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TextStyle {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

/// `styleToInlineCSS` (`ansi-to-html.ts:93-102`).
fn style_to_inline_css(style: &TextStyle) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(fg) = &style.fg {
        parts.push(format!("color:{fg}"));
    }
    if let Some(bg) = &style.bg {
        parts.push(format!("background-color:{bg}"));
    }
    if style.bold {
        parts.push("font-weight:bold".to_string());
    }
    if style.dim {
        parts.push("opacity:0.6".to_string());
    }
    if style.italic {
        parts.push("font-style:italic".to_string());
    }
    if style.underline {
        parts.push("text-decoration:underline".to_string());
    }
    parts.join(";")
}

/// `hasStyle` (`ansi-to-html.ts:104-106`).
fn has_style(style: &TextStyle) -> bool {
    style.fg.is_some()
        || style.bg.is_some()
        || style.bold
        || style.dim
        || style.italic
        || style.underline
}

/// `applySgrCode` (`ansi-to-html.ts:111-191`): parse SGR parameters and
/// update the style. Unrecognized codes are ignored.
fn apply_sgr_code(params: &[i64], style: &mut TextStyle) {
    let mut i = 0;
    while i < params.len() {
        let code = params[i];

        if code == 0 {
            // Reset all
            *style = TextStyle::default();
        } else if code == 1 {
            style.bold = true;
        } else if code == 2 {
            style.dim = true;
        } else if code == 3 {
            style.italic = true;
        } else if code == 4 {
            style.underline = true;
        } else if code == 22 {
            // Reset bold/dim
            style.bold = false;
            style.dim = false;
        } else if code == 23 {
            style.italic = false;
        } else if code == 24 {
            style.underline = false;
        } else if (30..=37).contains(&code) {
            // Standard foreground colors
            style.fg = Some(ANSI_COLORS[(code - 30) as usize].to_string());
        } else if code == 38 {
            // Extended foreground color
            if params.get(i + 1) == Some(&5) && params.len() > i + 2 {
                // 256-color: 38;5;N
                style.fg = Some(color256_to_hex(params[i + 2]));
                i += 2;
            } else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
                // RGB: 38;2;R;G;B
                let r = params[i + 2];
                let g = params[i + 3];
                let b = params[i + 4];
                style.fg = Some(format!("rgb({r},{g},{b})"));
                i += 4;
            }
        } else if code == 39 {
            // Default foreground
            style.fg = None;
        } else if (40..=47).contains(&code) {
            // Standard background colors
            style.bg = Some(ANSI_COLORS[(code - 40) as usize].to_string());
        } else if code == 48 {
            // Extended background color
            if params.get(i + 1) == Some(&5) && params.len() > i + 2 {
                // 256-color: 48;5;N
                style.bg = Some(color256_to_hex(params[i + 2]));
                i += 2;
            } else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
                // RGB: 48;2;R;G;B
                let r = params[i + 2];
                let g = params[i + 3];
                let b = params[i + 4];
                style.bg = Some(format!("rgb({r},{g},{b})"));
                i += 4;
            }
        } else if code == 49 {
            // Default background
            style.bg = None;
        } else if (90..=97).contains(&code) {
            // Bright foreground colors
            style.fg = Some(ANSI_COLORS[(code - 90 + 8) as usize].to_string());
        } else if (100..=107).contains(&code) {
            // Bright background colors
            style.bg = Some(ANSI_COLORS[(code - 100 + 8) as usize].to_string());
        }
        // Ignore unrecognized codes

        i += 1;
    }
}

/// `stripAnsiEscapes` (`ansi-to-html.ts:205-207`): remove all escape
/// sequences the converter matches, plus stray ESC characters.
pub fn strip_ansi_escapes(text: &str) -> String {
    let stripped = ansi_regex().replace_all(text, "");
    stripped.replace('\x1b', "")
}

/// `parseInt(p, 10) || 0` for an SGR parameter split. `[\d;]*` bounds the
/// input to digits and semicolons, so the only shapes are all-digits
/// (`parse` succeeds) and empty (`NaN` -> 0). A digit string too big for
/// `i64` parses to a huge JS number no SGR branch matches, so the `i64::MAX`
/// fallback preserves that "ignored" outcome instead of wrapping to a code
/// with meaning.
fn parse_sgr_param(part: &str) -> i64 {
    if part.is_empty() {
        0
    } else {
        part.parse::<i64>().unwrap_or(i64::MAX)
    }
}

/// `ansiToHtml` (`ansi-to-html.ts:212-289`): convert ANSI-escaped text to
/// HTML with inline styles.
pub fn ansi_to_html(text: &str) -> String {
    let mut style = TextStyle::default();
    let mut result = String::new();
    let mut in_link = false;

    let regex = ansi_regex();
    let mut last_index = 0;
    let mut in_span = false;
    for captures in regex.captures_iter(text) {
        let match_start = captures.get(0).expect("group 0").start();
        let match_end = captures.get(0).expect("group 0").end();
        // Add text before this escape sequence, minus stray ESC characters.
        let before_text = &text[last_index..match_start];
        if !before_text.is_empty() {
            result.push_str(&escape_html(&before_text.replace('\x1b', "")));
        }

        if let Some(params_group) = captures.get(1) {
            // Parse SGR parameters
            let param_str = params_group.as_str();
            let params: Vec<i64> = if param_str.is_empty() {
                vec![0]
            } else {
                param_str.split(';').map(parse_sgr_param).collect()
            };

            // closeSpan()
            if in_span {
                result.push_str("</span>");
                in_span = false;
            }
            apply_sgr_code(&params, &mut style);
            // openSpan()
            if has_style(&style) {
                result.push_str(&format!("<span style=\"{}\">", style_to_inline_css(&style)));
                in_span = true;
            }
        } else if let Some(osc_group) = captures.get(2) {
            if let Some(hyperlink) = osc8_regex().captures(osc_group.as_str()) {
                // Keep spans nested inside the link: close and reopen the
                // current style around link changes. Browsers strip C0
                // controls from URLs, so strip them before checking the
                // scheme.
                let url = hyperlink[1]
                    .trim()
                    .chars()
                    .filter(|character| !matches!(character, '\x00'..='\x1f' | '\x7f'))
                    .collect::<String>();
                if in_span {
                    result.push_str("</span>");
                    in_span = false;
                }
                if in_link {
                    result.push_str("</a>");
                    in_link = false;
                }
                if !url.is_empty() && safe_link_regex().is_match(&url) {
                    result.push_str(&format!("<a href=\"{}\">", escape_html(&url)));
                    in_link = true;
                }
                if has_style(&style) {
                    result.push_str(&format!("<span style=\"{}\">", style_to_inline_css(&style)));
                    in_span = true;
                }
            }
            // Other OSC sequences are dropped
        }
        // Other CSI and APC sequences are dropped

        last_index = match_end;
    }

    // Add remaining text
    let remaining_text = &text[last_index..];
    if !remaining_text.is_empty() {
        result.push_str(&escape_html(&remaining_text.replace('\x1b', "")));
    }

    // Close any open span and link
    if in_span {
        result.push_str("</span>");
    }
    if in_link {
        result.push_str("</a>");
    }

    result
}

/// `ansiLinesToHtml` (`ansi-to-html.ts:295-297`): each line is wrapped in a
/// div element; an empty line renders as `&nbsp;`.
pub fn ansi_lines_to_html(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| {
            let html = ansi_to_html(line);
            let html = if html.is_empty() { "&nbsp;" } else { &html };
            format!("<div class=\"ansi-line\">{html}</div>")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_survives_unstyled() {
        assert_eq!(ansi_to_html("hello world"), "hello world");
    }

    /// `export-html-whitespace.test.ts:26-30`: no source whitespace is
    /// inserted between ANSI-rendered lines.
    #[test]
    fn lines_join_without_separators() {
        assert_eq!(
            ansi_lines_to_html(&["one".to_string(), "two".to_string()]),
            "<div class=\"ansi-line\">one</div><div class=\"ansi-line\">two</div>"
        );
    }

    #[test]
    fn empty_lines_render_as_nbsp() {
        assert_eq!(
            ansi_lines_to_html(&["".to_string()]),
            "<div class=\"ansi-line\">&nbsp;</div>"
        );
    }

    #[test]
    fn colors_and_reset_produce_inline_spans() {
        // `export-html-whitespace.test.ts:35-43`: the trim case pins the
        // exact span the red foreground produces.
        assert_eq!(
            ansi_to_html("\u{1b}[31mone\u{1b}[0m"),
            "<span style=\"color:#800000\">one</span>"
        );
        assert_eq!(
            ansi_to_html("\u{1b}[1;38;5;196mx\u{1b}[0m"),
            "<span style=\"color:#ff0000;font-weight:bold\">x</span>"
        );
    }

    #[test]
    fn truecolor_and_bright_variants_render() {
        assert_eq!(
            ansi_to_html("\u{1b}[38;2;12;34;56mred\u{1b}[0m"),
            "<span style=\"color:rgb(12,34,56)\">red</span>"
        );
        assert_eq!(
            ansi_to_html("\u{1b}[91mbright\u{1b}[0m"),
            "<span style=\"color:#ff0000\">bright</span>"
        );
        assert_eq!(
            ansi_to_html("\u{1b}[101mbright-bg\u{1b}[0m"),
            "<span style=\"background-color:#ff0000\">bright-bg</span>"
        );
    }

    #[test]
    fn dim_italic_underline_and_combinations_render() {
        assert_eq!(
            ansi_to_html("\u{1b}[2mdim\u{1b}[0m"),
            "<span style=\"opacity:0.6\">dim</span>"
        );
        assert_eq!(
            ansi_to_html("\u{1b}[3mitalic\u{1b}[0m"),
            "<span style=\"font-style:italic\">italic</span>"
        );
        assert_eq!(
            ansi_to_html("\u{1b}[4munder\u{1b}[0m"),
            "<span style=\"text-decoration:underline\">under</span>"
        );
    }

    #[test]
    fn html_specials_are_escaped() {
        assert_eq!(
            ansi_to_html("<script>alert(\"x\")&'</script>"),
            "&lt;script&gt;alert(&quot;x&quot;)&amp;&#039;&lt;/script&gt;"
        );
    }

    #[test]
    fn other_csi_sequences_and_stray_esc_are_dropped() {
        assert_eq!(ansi_to_html("\u{1b}[2Jclear\u{1b}[Aup"), "clearup");
        assert_eq!(ansi_to_html("a\u{1b}b"), "ab");
        assert_eq!(strip_ansi_escapes("\u{1b}[31mx\u{1b}[0m\u{1b}y"), "xy");
    }

    #[test]
    fn osc8_hyperlinks_render_when_the_scheme_is_safe() {
        assert_eq!(
            ansi_to_html("\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7}"),
            "<a href=\"https://example.com\">link</a>"
        );
        // Unsafe schemes and empty URLs drop the anchor but keep the text.
        assert_eq!(
            ansi_to_html("\u{1b}]8;;javascript:alert(1)\u{7}bad\u{1b}]8;;\u{7}"),
            "bad"
        );
        assert_eq!(
            ansi_to_html("\u{1b}]8;;mailto:a@b.c\u{7}mail\u{1b}]8;;\u{7}"),
            "<a href=\"mailto:a@b.c\">mail</a>"
        );
    }

    #[test]
    fn osc_payloads_other_than_8_are_dropped() {
        assert_eq!(
            ansi_to_html("\u{1b}]0;title\u{7}body"),
            "body"
        );
    }

    #[test]
    fn grayscale_and_cube_palette_entries_convert() {
        // Cube 196 -> #ff0000, grayscale 241 -> #8e8e8e.
        assert_eq!(
            ansi_to_html("\u{1b}[38;5;196mc\u{1b}[0m"),
            "<span style=\"color:#ff0000\">c</span>"
        );
        assert_eq!(
            ansi_to_html("\u{1b}[38;5;241mg\u{1b}[0m"),
            "<span style=\"color:#8e8e8e\">g</span>"
        );
    }
}
