//! Colors: indexed/rgb/oklch values, mixing, 256-color downscaling, styling.
//!
//! Ground truth: `packages/tui/src/colors.ts` (367) and
//! `packages/tui/src/terminal-colors.ts` (91).

use crate::oklab::{
    Okhsl, Rgb, linear_srgb_to_rgb, okhsl_to_rgb, oklab_to_linear_srgb, rgb_to_okhsl, rgb_to_oklab,
};

/// 16 base colors. `colors.ts:150-166`.
const BASIC_COLORS: [Rgb; 16] = [
    Rgb {
        r: 0.0,
        g: 0.0,
        b: 0.0,
    },
    Rgb {
        r: 128.0,
        g: 0.0,
        b: 0.0,
    },
    Rgb {
        r: 0.0,
        g: 128.0,
        b: 0.0,
    },
    Rgb {
        r: 128.0,
        g: 128.0,
        b: 0.0,
    },
    Rgb {
        r: 0.0,
        g: 0.0,
        b: 128.0,
    },
    Rgb {
        r: 128.0,
        g: 0.0,
        b: 128.0,
    },
    Rgb {
        r: 0.0,
        g: 128.0,
        b: 128.0,
    },
    Rgb {
        r: 192.0,
        g: 192.0,
        b: 192.0,
    },
    Rgb {
        r: 128.0,
        g: 128.0,
        b: 128.0,
    },
    Rgb {
        r: 255.0,
        g: 0.0,
        b: 0.0,
    },
    Rgb {
        r: 0.0,
        g: 255.0,
        b: 0.0,
    },
    Rgb {
        r: 255.0,
        g: 255.0,
        b: 0.0,
    },
    Rgb {
        r: 0.0,
        g: 0.0,
        b: 255.0,
    },
    Rgb {
        r: 255.0,
        g: 0.0,
        b: 255.0,
    },
    Rgb {
        r: 0.0,
        g: 255.0,
        b: 255.0,
    },
    Rgb {
        r: 255.0,
        g: 255.0,
        b: 255.0,
    },
];

/// 6x6x6 color cube levels. `colors.ts:168`.
const CUBE_VALUES: [f64; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];

/// 24 grayscale ramp values. `colors.ts:169`.
fn gray_values() -> [f64; 24] {
    let mut values = [0.0; 24];
    for (index, value) in values.iter_mut().enumerate() {
        *value = 8.0 + index as f64 * 10.0;
    }
    values
}

/// A concrete color: every color converts to sRGB, so color math never fails.
/// `colors.ts:4-24`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    Indexed(u16),
    Rgb(Rgb),
    Oklch { l: f64, c: f64, h: f64 },
}

/// Terminal color capability. `colors.ts:25`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorMode {
    Color256,
    TrueColor,
}

/// Space `mix_colors` interpolates in. `colors.ts:26`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMixSpace {
    Oklch,
    Srgb,
}

/// OKLCH channels: lightness 0-1, chroma, hue in degrees. `colors.ts:28-36`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklch {
    pub l: f64,
    pub c: f64,
    pub h: f64,
}

/// Text attributes. `colors.ts:44-51`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
// One flag per text attribute, mirroring upstream's `TextAttributes`.
#[allow(clippy::struct_excessive_bools)]
pub struct TextAttributes {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strikethrough: bool,
}

/// Text style: attributes plus colors. `colors.ts:53-56`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TextStyle {
    pub attributes: TextAttributes,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

/// `colors.ts:62-67`.
pub fn indexed_color(index: u16) -> Color {
    Color::Indexed(index)
}

/// `colors.ts:69-79`.
pub fn rgb_color(r: f64, g: f64, b: f64) -> Color {
    Color::Rgb(Rgb { r, g, b })
}

/// `colors.ts:81-88`. pi wraps the hue into `[0, 360)` on the way in, so a
/// stored hue is always wrapped - including hues computed by [`mix_colors`],
/// which travel the short way around the wheel and land outside the range
/// before this wraps them back.
pub fn oklch_color(l: f64, c: f64, h: f64) -> Color {
    Color::Oklch {
        l,
        c,
        h: ((h % 360.0) + 360.0) % 360.0,
    }
}

/// `colors.ts:107-115`.
pub fn okhsl_color(h: f64, s: f64, l: f64) -> Color {
    rgb_color_to_color(okhsl_to_rgb(h, s, l))
}

fn rgb_color_to_color(rgb: Rgb) -> Color {
    Color::Rgb(rgb)
}

/// `colors.ts:117-119`.
pub fn color_to_okhsl(color: Color) -> Okhsl {
    rgb_to_okhsl(color_to_rgb(color))
}

/// `colors.ts:69-79`'s validation, lifted out of [`rgb_color`] so
/// [`parse_color`] can report the same errors pi throws.
fn checked_rgb(r: f64, g: f64, b: f64) -> Result<Color, String> {
    for (name, value) in [("r", r), ("g", g), ("b", b)] {
        if !value.is_finite() {
            return Err(format!("{name} must be finite"));
        }
        if !(0.0..=255.0).contains(&value) {
            return Err(format!("{name} must be between 0 and 255: {value}"));
        }
    }
    Ok(rgb_color(r, g, b))
}

/// `colors.ts:81-88`'s validation, hue wrap included.
fn checked_oklch(l: f64, c: f64, h: f64) -> Result<Color, String> {
    for (name, value) in [("l", l), ("c", c), ("h", h)] {
        if !value.is_finite() {
            return Err(format!("{name} must be finite"));
        }
    }
    if !(0.0..=1.0).contains(&l) {
        return Err(format!("l must be between 0 and 1: {l}"));
    }
    if c < 0.0 {
        return Err(format!("c must not be negative: {c}"));
    }
    Ok(oklch_color(l, c, h))
}

/// `colors.ts:107-115`'s validation, then the OKHSL -> sRGB conversion.
fn checked_okhsl(h: f64, s: f64, l: f64) -> Result<Color, String> {
    for (name, value) in [("h", h), ("s", s), ("l", l)] {
        if !value.is_finite() {
            return Err(format!("{name} must be finite"));
        }
    }
    if !(0.0..=1.0).contains(&s) {
        return Err(format!("s must be between 0 and 1: {s}"));
    }
    if !(0.0..=1.0).contains(&l) {
        return Err(format!("l must be between 0 and 1: {l}"));
    }
    Ok(okhsl_color(h, s, l))
}

/// A cursor over the inside of an `oklch(...)` / `okhsl(...)` argument list,
/// shaped like the two regexes at `colors.ts:91-98`. `NUMBER_PATTERN` is
/// `[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?`.
struct ColorArgs<'a> {
    rest: &'a str,
}

impl<'a> ColorArgs<'a> {
    fn new(body: &'a str) -> Self {
        // The patterns open with `\(\s*`.
        Self {
            rest: body.trim_start(),
        }
    }

    /// The patterns close with `\s*\)`, so whitespace before the paren is
    /// accepted (`oklch(0.5 0.1 30 )` parses).
    fn done(&self) -> bool {
        self.rest.trim().is_empty()
    }

    /// `NUMBER_PATTERN` anchored at the cursor.
    fn number(&mut self) -> Option<f64> {
        let bytes = self.rest.as_bytes();
        let mut index = 0usize;
        if matches!(bytes.first(), Some(b'+' | b'-')) {
            index += 1;
        }
        let int_digits = bytes[index..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        index += int_digits;
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let frac_digits = bytes[index..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            index += frac_digits;
            if int_digits == 0 && frac_digits == 0 {
                return None;
            }
        } else if int_digits == 0 {
            return None;
        }
        if matches!(bytes.get(index), Some(b'e' | b'E')) {
            let mut exponent = index + 1;
            if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
                exponent += 1;
            }
            let digits = bytes[exponent..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits == 0 {
                return None;
            }
            index = exponent + digits;
        }
        let (value, rest) = self.rest.split_at(index);
        self.rest = rest;
        value.parse::<f64>().ok()
    }

    /// An optional unit (`%` or `deg`), consumed only when it is really there.
    fn unit(&mut self, unit: &str) -> bool {
        match self.rest.strip_prefix(unit) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    /// The `\s+` between two channels: at least one space is required.
    fn required_space(&mut self) -> bool {
        let trimmed = self.rest.trim_start();
        if trimmed.len() == self.rest.len() {
            return false;
        }
        self.rest = trimmed;
        true
    }
}

/// `parseColor`, `colors.ts:121-148`. `#rgb`, `#rrggbb`, `oklch()` and
/// `okhsl()`, all case-insensitively; anything else is an error.
///
/// pi anchors every pattern, so surrounding whitespace is not tolerated, and
/// the range checks live in the constructors - which is why the errors here
/// are pi's constructor messages rather than the pattern's. A `%` divides
/// that channel by 100; `deg` after a hue is accepted and dropped, because a
/// hue is always degrees.
pub fn parse_color(value: &str) -> Result<Color, String> {
    let invalid = || format!("Invalid color value: {value}");

    if let Some(digits) = value.strip_prefix('#') {
        let lower = digits.to_ascii_lowercase();
        let expanded = match lower.len() {
            3 if lower.bytes().all(|byte| byte.is_ascii_hexdigit()) => lower
                .chars()
                .flat_map(|digit| [digit, digit])
                .collect::<String>(),
            6 if lower.bytes().all(|byte| byte.is_ascii_hexdigit()) => lower,
            _ => return Err(invalid()),
        };
        let component = |range: std::ops::Range<usize>| -> Result<f64, String> {
            u8::from_str_radix(&expanded[range], 16)
                .map(f64::from)
                .map_err(|_| invalid())
        };
        return checked_rgb(component(0..2)?, component(2..4)?, component(4..6)?);
    }

    let lower = value.to_ascii_lowercase();
    if let Some(body) = lower
        .strip_prefix("oklch(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let mut args = ColorArgs::new(body);
        let lightness = args.number().ok_or_else(invalid)?;
        let lightness_percent = args.unit("%");
        if !args.required_space() {
            return Err(invalid());
        }
        let chroma = args.number().ok_or_else(invalid)?;
        if !args.required_space() {
            return Err(invalid());
        }
        let hue = args.number().ok_or_else(invalid)?;
        args.unit("deg");
        if !args.done() {
            return Err(invalid());
        }
        let lightness = if lightness_percent {
            lightness / 100.0
        } else {
            lightness
        };
        return checked_oklch(lightness, chroma, hue);
    }

    if let Some(body) = lower
        .strip_prefix("okhsl(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let mut args = ColorArgs::new(body);
        let hue = args.number().ok_or_else(invalid)?;
        args.unit("deg");
        if !args.required_space() {
            return Err(invalid());
        }
        let saturation = args.number().ok_or_else(invalid)?;
        let saturation_percent = args.unit("%");
        if !args.required_space() {
            return Err(invalid());
        }
        let lightness = args.number().ok_or_else(invalid)?;
        let lightness_percent = args.unit("%");
        if !args.done() {
            return Err(invalid());
        }
        let saturation = if saturation_percent {
            saturation / 100.0
        } else {
            saturation
        };
        let lightness = if lightness_percent {
            lightness / 100.0
        } else {
            lightness
        };
        return checked_okhsl(hue, saturation, lightness);
    }

    Err(invalid())
}

/// `parseColor`'s numeric overload (`colors.ts:123`): pi routes a number
/// through `indexedColor`, so the 0-255 integer check applies. `u16` already
/// rules out negatives and fractions; anything above 255 fails with pi's
/// message.
pub fn parse_color_index(value: u16) -> Result<Color, String> {
    if value > 255 {
        return Err(format!(
            "ANSI color index must be an integer from 0 to 255: {value}"
        ));
    }
    Ok(indexed_color(value))
}

/// `colors.ts:171-183`.
fn indexed_to_rgb(index: u16) -> Rgb {
    if index < 16 {
        return BASIC_COLORS[index as usize];
    }
    if index < 232 {
        let cube = index - 16;
        return Rgb {
            r: CUBE_VALUES[(cube / 36) as usize],
            g: CUBE_VALUES[((cube % 36) / 6) as usize],
            b: CUBE_VALUES[(cube % 6) as usize],
        };
    }
    let gray = 8.0 + (index - 232) as f64 * 10.0;
    Rgb {
        r: gray,
        g: gray,
        b: gray,
    }
}

fn is_in_srgb_gamut(linear: [f64; 3]) -> bool {
    const EPSILON: f64 = 1e-7;
    linear
        .iter()
        .all(|channel| *channel >= -EPSILON && *channel <= 1.0 + EPSILON)
}

/// `colors.ts:190-214`. The hue stays fixed and chroma is bisected down until
/// the color fits, so `oklch(100% 0.3 150)` maps to white instead of failing.
fn oklch_to_rgb(l: f64, c: f64, h: f64) -> Rgb {
    let radians = h.to_radians();
    let cos = radians.cos();
    let sin = radians.sin();
    let at_chroma = |chroma: f64| oklab_to_linear_srgb([l, chroma * cos, chroma * sin]);

    let direct = at_chroma(c);
    if is_in_srgb_gamut(direct) {
        return linear_srgb_to_rgb(direct);
    }
    let mut linear = at_chroma(0.0);
    let mut low = 0.0;
    let mut high = c;
    for _ in 0..20 {
        // `f64::midpoint` is stable only since 1.87; workspace MSRV is 1.85.
        #[allow(clippy::manual_midpoint)]
        let chroma = (low + high) / 2.0;
        let candidate = at_chroma(chroma);
        if is_in_srgb_gamut(candidate) {
            low = chroma;
            linear = candidate;
        } else {
            high = chroma;
        }
    }
    linear_srgb_to_rgb(linear)
}

/// `colors.ts:218-227`.
pub fn color_to_rgb(color: Color) -> Rgb {
    match color {
        Color::Indexed(index) => indexed_to_rgb(index),
        Color::Rgb(rgb) => rgb,
        Color::Oklch { l, c, h } => oklch_to_rgb(l, c, h),
    }
}

/// `colors.ts:229-233`.
pub fn color_to_oklch(color: Color) -> Oklch {
    match color {
        Color::Oklch { l, c, h } => Oklch { l, c, h },
        other => {
            let [l, a, b] = rgb_to_oklab(color_to_rgb(other));
            Oklch {
                l,
                c: a.hypot(b),
                h: (b.atan2(a).to_degrees() + 360.0) % 360.0,
            }
        }
    }
}

/// `colors.ts:235-239`.
pub fn color_to_hex(color: Color) -> String {
    let Rgb { r, g, b } = color_to_rgb(color);
    let channel = |value: f64| format!("{:02x}", value.round() as u8);
    format!("#{}{}{}", channel(r), channel(g), channel(b))
}

/// `colors.ts:241-257`. Oklch by default: an achromatic endpoint lends its hue
/// to the other one, and hue travels the short way around the wheel.
pub fn mix_colors(
    first: Color,
    second: Color,
    amount: f64,
    space: ColorMixSpace,
) -> Result<Color, String> {
    if !amount.is_finite() {
        return Err("amount must be finite".to_string());
    }
    if !(0.0..=1.0).contains(&amount) {
        return Err(format!("amount must be between 0 and 1: {amount}"));
    }
    if space == ColorMixSpace::Srgb {
        let a = color_to_rgb(first);
        let b = color_to_rgb(second);
        return Ok(rgb_color(
            a.r + (b.r - a.r) * amount,
            a.g + (b.g - a.g) * amount,
            a.b + (b.b - a.b) * amount,
        ));
    }
    let a = color_to_oklch(first);
    let b = color_to_oklch(second);
    let first_hue = if a.c < 1e-7 { b.h } else { a.h };
    let second_hue = if b.c < 1e-7 { first_hue } else { b.h };
    let hue_delta = ((second_hue - first_hue + 540.0) % 360.0) - 180.0;
    Ok(oklch_color(
        a.l + (b.l - a.l) * amount,
        a.c + (b.c - a.c) * amount,
        first_hue + hue_delta * amount,
    ))
}

fn find_closest(values: &[f64], target: f64) -> usize {
    let mut closest_index = 0;
    let mut closest_distance = f64::INFINITY;
    for (index, value) in values.iter().enumerate() {
        let distance = (target - value).abs();
        if distance < closest_distance {
            closest_index = index;
            closest_distance = distance;
        }
    }
    closest_index
}

/// Perceptual distance weights. `colors.ts:272-277`.
fn color_distance(first: Rgb, second: Rgb) -> f64 {
    let dr = first.r - second.r;
    let dg = first.g - second.g;
    let db = first.b - second.b;
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// `colors.ts:279-297`. Nearest cube entry, unless a gray is perceptually
/// closer, which keeps neutral theme colors neutral after downscaling.
fn rgb_to_ansi256(color: Rgb) -> u16 {
    let r_index = find_closest(&CUBE_VALUES, color.r);
    let g_index = find_closest(&CUBE_VALUES, color.g);
    let b_index = find_closest(&CUBE_VALUES, color.b);
    let cube_color = Rgb {
        r: CUBE_VALUES[r_index],
        g: CUBE_VALUES[g_index],
        b: CUBE_VALUES[b_index],
    };
    let cube_index = 16 + 36 * r_index + 6 * g_index + b_index;

    let gray = (0.299 * color.r + 0.587 * color.g + 0.114 * color.b).round();
    let grays = gray_values();
    let gray_offset = find_closest(&grays, gray);
    let gray_value = grays[gray_offset];
    let spread = color.r.max(color.g).max(color.b) - color.r.min(color.g).min(color.b);
    if spread < 10.0
        && color_distance(
            color,
            Rgb {
                r: gray_value,
                g: gray_value,
                b: gray_value,
            },
        ) < color_distance(color, cube_color)
    {
        return 232 + gray_offset as u16;
    }
    cube_index as u16
}

/// `colors.ts:299-315`.
fn color_ansi(color: Color, mode: TerminalColorMode, background: bool) -> String {
    if let Color::Indexed(index) = color {
        return format!("\x1b[{};5;{}m", if background { 48 } else { 38 }, index);
    }
    let Rgb { r, g, b } = color_to_rgb(color);
    let prefix = if background { 48 } else { 38 };
    match mode {
        TerminalColorMode::TrueColor => format!(
            "\x1b[{};2;{};{};{}m",
            prefix,
            r.round() as u8,
            g.round() as u8,
            b.round() as u8
        ),
        TerminalColorMode::Color256 => {
            format!("\x1b[{};5;{}m", prefix, rgb_to_ansi256(Rgb { r, g, b }))
        }
    }
}

/// `colors.ts:309-311`.
pub fn foreground_ansi(color: Color, mode: TerminalColorMode) -> String {
    color_ansi(color, mode, false)
}

/// `colors.ts:313-315`.
pub fn background_ansi(color: Color, mode: TerminalColorMode) -> String {
    color_ansi(color, mode, true)
}

/// `colors.ts:317-328`.
pub fn style_text(text: &str, style: TextStyle, mode: TerminalColorMode) -> String {
    let fg = style.fg.map(|color| foreground_ansi(color, mode));
    let bg = style.bg.map(|color| background_ansi(color, mode));
    style_text_with_ansi(text, fg.as_deref(), bg.as_deref(), style.attributes)
}

/// `colors.ts:330-367`. Resets are prepended so they close in reverse order
/// of the opening sequences.
pub fn style_text_with_ansi(
    text: &str,
    fg_ansi: Option<&str>,
    bg_ansi: Option<&str>,
    attributes: TextAttributes,
) -> String {
    let mut prefix = String::new();
    let mut suffix = String::new();
    if let Some(code) = fg_ansi {
        prefix.push_str(code);
        suffix = "\x1b[39m".to_string();
    }
    if let Some(code) = bg_ansi {
        prefix.push_str(code);
        suffix = format!("\x1b[49m{suffix}");
    }
    if attributes.bold {
        prefix.push_str("\x1b[1m");
    }
    if attributes.dim {
        prefix.push_str("\x1b[2m");
    }
    if attributes.bold || attributes.dim {
        suffix = format!("\x1b[22m{suffix}");
    }
    if attributes.italic {
        prefix.push_str("\x1b[3m");
        suffix = format!("\x1b[23m{suffix}");
    }
    if attributes.underline {
        prefix.push_str("\x1b[4m");
        suffix = format!("\x1b[24m{suffix}");
    }
    if attributes.inverse {
        prefix.push_str("\x1b[7m");
        suffix = format!("\x1b[27m{suffix}");
    }
    if attributes.strikethrough {
        prefix.push_str("\x1b[9m");
        suffix = format!("\x1b[29m{suffix}");
    }
    format!("{prefix}{text}{suffix}")
}

/// Colors the terminal reports for its current theme. `terminal-colors.ts:10-17`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalColors {
    /// Default foreground (OSC 10).
    pub foreground: Option<Rgb>,
    /// Default background (OSC 11).
    pub background: Option<Rgb>,
    /// ANSI colors 0-15 (OSC 4); only set when all 16 arrived.
    pub palette: Option<Vec<Rgb>>,
}

/// Whether the terminal is in a light or dark scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorScheme {
    Dark,
    Light,
}

/// What an OSC color reply reports. `terminal-colors.ts:39`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscColorTarget {
    Foreground,
    Background,
    Palette(u16),
}

/// Result of parsing an OSC 10/11/4 reply.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OscColorResponse {
    pub target: OscColorTarget,
    pub rgb: Option<Rgb>,
}

fn hex_to_rgb(hex: &str) -> Option<Rgb> {
    if hex.len() < 6 {
        return None;
    }
    let parse = |slice: &str| u8::from_str_radix(slice, 16).ok().map(|v| v as f64);
    Some(Rgb {
        r: parse(&hex[0..2])?,
        g: parse(&hex[2..4])?,
        b: parse(&hex[4..6])?,
    })
}

/// `terminal-colors.ts:27-36`.
fn parse_osc_hex_channel(channel: &str) -> Option<f64> {
    if channel.is_empty() || !channel.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16f64.powi(channel.len() as i32) - 1.0;
    if max <= 0.0 {
        return None;
    }
    let value = u64::from_str_radix(channel, 16).ok()? as f64;
    Some((value / max * 255.0).round())
}

/// `terminal-colors.ts:58-83`.
fn parse_osc_color_value(raw: &str) -> Option<Rgb> {
    let value = raw.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 {
            return hex_to_rgb(hex);
        }
        if hex.len() == 12 {
            let r = parse_osc_hex_channel(&hex[0..4])?;
            let g = parse_osc_hex_channel(&hex[4..8])?;
            let b = parse_osc_hex_channel(&hex[8..12])?;
            return Some(Rgb { r, g, b });
        }
        return None;
    }
    let rgb_value = strip_rgb_prefix(value);
    let mut parts = rgb_value.splitn(3, '/');
    let red = parts.next()?;
    let green = parts.next()?;
    let blue = parts.next()?;
    Some(Rgb {
        r: parse_osc_hex_channel(red)?,
        g: parse_osc_hex_channel(green)?,
        b: parse_osc_hex_channel(blue)?,
    })
}

fn strip_rgb_prefix(value: &str) -> &str {
    for prefix in ["rgba:", "rgb:", "RGBA:", "RGB:"] {
        if let Some(rest) = value.strip_prefix(prefix) {
            return rest;
        }
    }
    value
}

/// `terminal-colors.ts:48-56`. Returns `None` when `data` is not an OSC color
/// reply at all; `rgb` is `None` for a reply with an unparseable color.
pub fn parse_osc_color_response(data: &str) -> Option<OscColorResponse> {
    let body = data.strip_prefix('\x1b')?;
    let body = body.strip_prefix(']')?;
    let body = body
        .strip_suffix('\x07')
        .or_else(|| body.strip_suffix("\x1b\\"))?;
    let (head, rest) = body.split_once(';')?;
    // OSC 4 carries the palette index and the color: `\x1b]4;<index>;<color>`.
    let (target, value) = if head == "4" {
        let (index, color) = rest.split_once(';')?;
        (OscColorTarget::Palette(index.parse().ok()?), color)
    } else if head == "10" {
        (OscColorTarget::Foreground, rest)
    } else if head == "11" {
        (OscColorTarget::Background, rest)
    } else {
        return None;
    };
    if value.contains('\x1b') {
        return None;
    }
    Some(OscColorResponse {
        target,
        rgb: parse_osc_color_value(value),
    })
}

/// `terminal-colors.ts:85-91`. Reports may be concatenated.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalColorScheme> {
    if data.is_empty() || !data.starts_with("\x1b[?997;") {
        return None;
    }
    let mut rest = data;
    let mut scheme = None;
    while let Some(after) = rest.strip_prefix("\x1b[?997;") {
        let end = after.find('n')?;
        match &after[..end] {
            "1" => scheme = Some(TerminalColorScheme::Dark),
            "2" => scheme = Some(TerminalColorScheme::Light),
            _ => return None,
        }
        rest = &after[end + 1..];
    }
    if rest.is_empty() { scheme } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, epsilon: f64) -> bool {
        (a - b).abs() <= epsilon
    }

    #[test]
    fn indexed_colors_match_palette() {
        assert_eq!(
            color_to_rgb(indexed_color(1)),
            Rgb {
                r: 128.0,
                g: 0.0,
                b: 0.0
            }
        );
        assert_eq!(
            color_to_rgb(indexed_color(16)),
            Rgb {
                r: 0.0,
                g: 0.0,
                b: 0.0
            }
        );
        // 196 is the cube entry for pure red.
        assert_eq!(
            color_to_rgb(indexed_color(196)),
            Rgb {
                r: 255.0,
                g: 0.0,
                b: 0.0
            }
        );
        assert_eq!(
            color_to_rgb(indexed_color(232)),
            Rgb {
                r: 8.0,
                g: 8.0,
                b: 8.0
            }
        );
        assert_eq!(
            color_to_rgb(indexed_color(255)),
            Rgb {
                r: 238.0,
                g: 238.0,
                b: 238.0
            }
        );
    }

    #[test]
    fn oklch_maps_into_gamut_by_reducing_chroma() {
        let in_gamut = color_to_rgb(oklch_color(0.6, 0.1, 30.0));
        assert!(in_gamut.r > 0.0 && in_gamut.r <= 255.0);
        // `oklch(100% 0.3 150)` must fall back to white, never panic or clip weirdly.
        let white = color_to_rgb(oklch_color(1.0, 0.3, 150.0));
        assert_eq!(
            white,
            Rgb {
                r: 255.0,
                g: 255.0,
                b: 255.0
            }
        );
    }

    #[test]
    fn color_to_hex_formats_two_digits() {
        assert_eq!(color_to_hex(rgb_color(255.0, 0.0, 0.0)), "#ff0000");
        assert_eq!(color_to_hex(indexed_color(7)), "#c0c0c0");
    }

    #[test]
    fn mix_rejects_out_of_range_amounts() {
        assert!(
            mix_colors(
                indexed_color(0),
                indexed_color(7),
                1.5,
                ColorMixSpace::Oklch
            )
            .is_err()
        );
        assert!(
            mix_colors(
                indexed_color(0),
                indexed_color(7),
                f64::NAN,
                ColorMixSpace::Oklch
            )
            .is_err()
        );
        assert!(
            mix_colors(
                indexed_color(0),
                indexed_color(7),
                0.0,
                ColorMixSpace::Oklch
            )
            .is_ok()
        );
    }

    #[test]
    fn mix_endpoints_and_midpoint() {
        let black = indexed_color(0);
        let white = indexed_color(7);
        let start = mix_colors(black, white, 0.0, ColorMixSpace::Oklch).unwrap();
        let end = mix_colors(black, white, 1.0, ColorMixSpace::Oklch).unwrap();
        assert!(close(color_to_rgb(start).r, 0.0, 0.001));
        assert!(close(color_to_rgb(end).r, 192.0, 0.001));

        let mid = mix_colors(black, white, 0.5, ColorMixSpace::Srgb).unwrap();
        let rgb = color_to_rgb(mid);
        assert!(close(rgb.r, 96.0, 0.001) && close(rgb.g, 96.0, 0.001));
    }

    #[test]
    fn mix_borrows_hue_from_the_chromatic_endpoint() {
        let grey = rgb_color(128.0, 128.0, 128.0);
        let red = rgb_color(255.0, 0.0, 0.0);
        let mixed = mix_colors(grey, red, 0.5, ColorMixSpace::Oklch).unwrap();
        let oklch = color_to_oklch(mixed);
        // Red's hue survives even though grey has no chroma of its own.
        assert!(oklch.h < 45.0, "hue {}", oklch.h);
        assert!(oklch.c > 0.05);
    }

    #[test]
    fn truecolor_ansi_is_direct() {
        assert_eq!(
            foreground_ansi(rgb_color(1.0, 2.0, 3.0), TerminalColorMode::TrueColor),
            "\x1b[38;2;1;2;3m"
        );
        assert_eq!(
            background_ansi(rgb_color(1.0, 2.0, 3.0), TerminalColorMode::TrueColor),
            "\x1b[48;2;1;2;3m"
        );
        assert_eq!(
            foreground_ansi(indexed_color(42), TerminalColorMode::TrueColor),
            "\x1b[38;5;42m"
        );
    }

    #[test]
    fn color256_downscales_and_keeps_neutrals_gray() {
        // Pure red survives the cube path.
        assert_eq!(
            foreground_ansi(rgb_color(255.0, 0.0, 0.0), TerminalColorMode::Color256),
            "\x1b[38;5;196m"
        );
        // A near-grey picks the gray ramp, not a cube entry.
        assert_eq!(
            foreground_ansi(rgb_color(128.0, 128.0, 128.0), TerminalColorMode::Color256),
            "\x1b[38;5;244m"
        );
    }

    #[test]
    fn style_text_closes_codes_in_reverse_order() {
        let styled = style_text(
            "x",
            TextStyle {
                attributes: TextAttributes {
                    bold: true,
                    underline: true,
                    ..Default::default()
                },
                fg: Some(indexed_color(1)),
                bg: Some(indexed_color(4)),
            },
            TerminalColorMode::Color256,
        );
        assert_eq!(
            styled,
            "\x1b[38;5;1m\x1b[48;5;4m\x1b[1m\x1b[4mx\x1b[24m\x1b[22m\x1b[49m\x1b[39m"
        );
    }

    #[test]
    fn style_text_with_precomputed_ansi_ignores_colors_in_attributes() {
        let styled =
            style_text_with_ansi("hi", Some("\x1b[38;5;9m"), None, TextAttributes::default());
        assert_eq!(styled, "\x1b[38;5;9mhi\x1b[39m");
    }

    #[test]
    fn parses_osc_color_replies() {
        let fg = parse_osc_color_response("\x1b]10;rgb:ff/00/00\x07").unwrap();
        assert_eq!(fg.target, OscColorTarget::Foreground);
        assert_eq!(
            fg.rgb,
            Some(Rgb {
                r: 255.0,
                g: 0.0,
                b: 0.0
            })
        );

        let bg = parse_osc_color_response("\x1b]11;#123456\x1b\\").unwrap();
        assert_eq!(bg.target, OscColorTarget::Background);
        assert_eq!(
            bg.rgb,
            Some(Rgb {
                r: 18.0,
                g: 52.0,
                b: 86.0
            })
        );

        let palette = parse_osc_color_response("\x1b]4;3;#ffffff\x07").unwrap();
        assert_eq!(palette.target, OscColorTarget::Palette(3));
        assert_eq!(
            palette.rgb,
            Some(Rgb {
                r: 255.0,
                g: 255.0,
                b: 255.0
            })
        );

        // 12-digit hex scales each channel.
        let wide = parse_osc_color_response("\x1b]4;3;ffff/0000/0000\x07").unwrap();
        assert_eq!(
            wide.rgb,
            Some(Rgb {
                r: 255.0,
                g: 0.0,
                b: 0.0
            })
        );

        // Not a color reply at all.
        assert!(parse_osc_color_response("\x1b[A").is_none());
        // A reply with an unparseable color keeps the target.
        let broken = parse_osc_color_response("\x1b]10;notacolor\x07").unwrap();
        assert_eq!(broken.target, OscColorTarget::Foreground);
        assert_eq!(broken.rgb, None);
    }

    #[test]
    fn parses_color_scheme_reports() {
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;2n\x1b[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(parse_terminal_color_scheme_report("\x1b[A"), None);
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;3n"), None);
    }

    #[test]
    fn okhsl_color_round_trips() {
        let color = okhsl_color(120.0, 0.7, 0.4);
        let back = color_to_okhsl(color);
        assert!(close(back.h, 120.0, 1.0));
        assert!(close(back.s, 0.7, 0.05));
    }

    /// The expected values here are pi's own, read out of `parseColor` run
    /// over `packages/tui/src/colors.ts` under bun, one call per row.
    #[test]
    fn parse_color_matches_pi_on_every_accepted_spelling() {
        let cases: Vec<(&str, Color)> = vec![
            ("#f00", rgb_color(255.0, 0.0, 0.0)),
            ("#F00", rgb_color(255.0, 0.0, 0.0)),
            ("#ff0000", rgb_color(255.0, 0.0, 0.0)),
            ("#FF0000", rgb_color(255.0, 0.0, 0.0)),
            ("#abc", rgb_color(170.0, 187.0, 204.0)),
            ("#aabbcc", rgb_color(170.0, 187.0, 204.0)),
            ("oklch(0.5 0.1 30)", oklch_color(0.5, 0.1, 30.0)),
            ("oklch(50% 0.1 30)", oklch_color(0.5, 0.1, 30.0)),
            ("oklch(0.5 0.1 30deg)", oklch_color(0.5, 0.1, 30.0)),
            ("OKLCH(0.5 0.1 30)", oklch_color(0.5, 0.1, 30.0)),
            ("oklch(.5 .1 3e1)", oklch_color(0.5, 0.1, 30.0)),
            // The hue is wrapped into [0, 360), never negative.
            ("oklch(0.5 0.1 -30)", oklch_color(0.5, 0.1, 330.0)),
            ("oklch(0.5 0.1 390)", oklch_color(0.5, 0.1, 30.0)),
            // okhsl converts to sRGB on the way in.
            ("okhsl(30 0.5 0.5)", rgb_color(169.0, 95.0, 84.0)),
            ("okhsl(30deg 50% 50%)", rgb_color(169.0, 95.0, 84.0)),
            ("OKHSL(30 0.5 0.5)", rgb_color(169.0, 95.0, 84.0)),
            ("okhsl(30 50% .5)", rgb_color(169.0, 95.0, 84.0)),
            // pi's `\s*` before `)` accepts trailing whitespace inside.
            ("oklch(0.5 0.1 30 )", oklch_color(0.5, 0.1, 30.0)),
            ("oklch( 0.5 0.1 30)", oklch_color(0.5, 0.1, 30.0)),
            ("oklch(0.5 0.1 30deg )", oklch_color(0.5, 0.1, 30.0)),
            ("okhsl(30 0.5 0.5 )", rgb_color(169.0, 95.0, 84.0)),
        ];
        for (text, expected) in cases {
            let parsed =
                parse_color(text).unwrap_or_else(|error| panic!("{text:?} rejected: {error}"));
            match (&parsed, &expected) {
                (Color::Rgb(a), Color::Rgb(b)) => {
                    assert!(
                        close(a.r, b.r, 0.5) && close(a.g, b.g, 0.5) && close(a.b, b.b, 0.5),
                        "{text:?} parsed to {a:?}, pi gives {b:?}"
                    );
                }
                (a, b) => assert_eq!(a, b, "{text:?} parsed to {a:?}, pi gives {b:?}"),
            }
        }
    }

    /// Same pi run: the rejected spellings, with the messages pi throws. The
    /// last two come from pi's constructors, not from its patterns.
    #[test]
    fn parse_color_rejects_what_pi_rejects() {
        let cases: Vec<(&str, &str)> = vec![
            ("red", "Invalid color value: red"),
            ("", "Invalid color value: "),
            ("#ff00", "Invalid color value: #ff00"),
            ("#gg0000", "Invalid color value: #gg0000"),
            // 12-bit hex is an OSC reply spelling, not `parseColor`'s.
            ("#aabbccddeeff", "Invalid color value: #aabbccddeeff"),
            ("rgb(1,2,3)", "Invalid color value: rgb(1,2,3)"),
            ("oklch(0.5 0.1)", "Invalid color value: oklch(0.5 0.1)"),
            ("okhsl(30 0.5)", "Invalid color value: okhsl(30 0.5)"),
            (
                "oklch(0.5, 0.1, 30)",
                "Invalid color value: oklch(0.5, 0.1, 30)",
            ),
            // Every pattern is anchored at both ends.
            (" #f00", "Invalid color value:  #f00"),
            ("#f00 ", "Invalid color value: #f00 "),
            (
                "oklch(0.5 0.1 30) ",
                "Invalid color value: oklch(0.5 0.1 30) ",
            ),
            (
                "oklch(0.5 0.1 30)x",
                "Invalid color value: oklch(0.5 0.1 30)x",
            ),
            ("oklch()", "Invalid color value: oklch()"),
            ("okhsl()", "Invalid color value: okhsl()"),
            ("0x10", "Invalid color value: 0x10"),
            // pi's own range checks.
            ("oklch(+0.5 -0.1 30)", "c must not be negative: -0.1"),
            ("oklch(2 0.1 30)", "l must be between 0 and 1: 2"),
            ("okhsl(30 160% 55%)", "s must be between 0 and 1: 1.6"),
            ("okhsl(30 0.5 250%)", "l must be between 0 and 1: 2.5"),
        ];
        for (text, message) in cases {
            assert_eq!(parse_color(text).unwrap_err(), message, "for {text:?}");
        }
    }

    #[test]
    fn oklch_constructor_wraps_hue_like_pi() {
        // pi stores the wrapped hue (`colors.ts:81-88`); `mix_colors` relies
        // on it for hues that cross zero.
        assert_eq!(oklch_color(0.5, 0.1, 390.0), oklch_color(0.5, 0.1, 30.0));
        assert_eq!(oklch_color(0.5, 0.1, -30.0), oklch_color(0.5, 0.1, 330.0));
    }

    #[test]
    fn parse_color_index_rejects_what_indexed_color_rejects() {
        assert_eq!(parse_color_index(42).unwrap(), indexed_color(42));
        assert_eq!(parse_color_index(255).unwrap(), indexed_color(255));
        assert_eq!(
            parse_color_index(300).unwrap_err(),
            "ANSI color index must be an integer from 0 to 255: 300"
        );
    }
}
