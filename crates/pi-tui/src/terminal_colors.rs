//! Terminal colour query protocol: OSC 10/11/4 replies, the CSI ?997 colour
//! scheme report and the CSI 16 t cell-size reply.
//!
//! Ground truth: `packages/tui/src/terminal-colors.ts` (parsers) and
//! `packages/tui/src/tui.ts:160-172` (the query bytes), `:960-970`
//! (`queryCellSize`), `:1122-1200` (the three consumers).
//!
//! Parsing lives here; the orchestration — when to emit, when to give up, how
//! to hand late replies to the caller — lives in [`crate::tui::Tui`], because
//! this crate owns no timer: the timeout is a deadline the host services.

/// `TERMINAL_PALETTE_SIZE`, `tui.ts:160`.
pub const TERMINAL_PALETTE_SIZE: usize = 16;
/// OSC 10 and 11 plus OSC 4 for every palette colour. `tui.ts:162`.
pub const TERMINAL_COLOR_REPLY_COUNT: usize = 2 + TERMINAL_PALETTE_SIZE;

/// The query pi writes: default fg, default bg, the 16 palette colours, then a
/// primary device attributes request. Every terminal answers DA1 and answers
/// the colour queries in order, so the DA1 reply fences the batch — including
/// for terminals that ignore the queries entirely. `tui.ts:163-171`.
pub fn terminal_color_query() -> String {
    let mut query = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for index in 0..TERMINAL_PALETTE_SIZE {
        {
            use std::fmt::Write as _;
            let _ = write!(query, "\x1b]4;{index};?\x07");
        }
    }
    query.push_str("\x1b[c");
    query
}

/// `DEVICE_ATTRIBUTES_RESPONSE_PATTERN`, `tui.ts:172`.
pub fn is_device_attributes_response(data: &str) -> bool {
    let Some(rest) = data.strip_prefix("\x1b[?") else {
        return false;
    };
    let Some(body) = rest.strip_suffix('c') else {
        return false;
    };
    !body.is_empty()
        && body
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b';')
}

/// `CSI 16 t` requests the cell size in pixels. `tui.ts:960-970`.
pub const CELL_SIZE_QUERY: &str = "\x1b[16t";

/// `DEC private mode 2031`: ask the terminal to report its colour scheme, and
/// with it whether it is dark or light. `tui.ts:925-927` writes it on start,
/// `:956` toggles it and `:974` drops it on stop; the replies it produces are
/// the `CSI ?997;n n` reports [`parse_terminal_color_scheme_report`] reads.
pub const COLOR_SCHEME_MODE_ON: &str = "\x1b[?2031h";
/// The off half of [`COLOR_SCHEME_MODE_ON`]. `tui.ts:956`, `:974`.
pub const COLOR_SCHEME_MODE_OFF: &str = "\x1b[?2031l";

/// A reported colour. `terminal-colors.ts:1-5`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// `TerminalColorScheme`, `terminal-colors.ts:7`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalColorScheme {
    Dark,
    Light,
}

/// What an OSC reply reports. `terminal-colors.ts:38`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscColorTarget {
    Foreground,
    Background,
    Palette(usize),
}

/// `parseOscColorResponse`, `terminal-colors.ts:48-56`. `rgb` is `None` for a
/// reply whose colour does not parse — the reply still counts as handled.
pub fn parse_osc_color_response(data: &str) -> Option<(OscColorTarget, Option<RgbColor>)> {
    let body = data.strip_prefix("\x1b]")?;
    let body = strip_terminator(body)?;
    let (code, rest) = body.split_once(';')?;
    match code {
        "10" => Some((OscColorTarget::Foreground, parse_osc_color_value(rest))),
        "11" => Some((OscColorTarget::Background, parse_osc_color_value(rest))),
        "4" => {
            // The palette form carries the index as a third field: `4;N;colour`.
            let (index_text, colour) = rest.split_once(';')?;
            if index_text.is_empty()
                || index_text.len() > 3
                || !index_text.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let index: usize = index_text.parse().ok()?;
            Some((
                OscColorTarget::Palette(index),
                parse_osc_color_value(colour),
            ))
        }
        _ => None,
    }
}

/// OSC replies end in BEL or ST. `terminal-colors.ts:44`.
fn strip_terminator(body: &str) -> Option<&str> {
    body.strip_suffix('\x07')
        .or_else(|| body.strip_suffix("\x1b\\"))
}

/// `parseOscColorValue`, `terminal-colors.ts:58-88`. Terminals report either
/// `#rrggbb`, `#rrrgggbbb` (12-bit, equal-width channels) or `rgb:RR/GG/BB`
/// with per-channel bit depths.
pub fn parse_osc_color_value(raw: &str) -> Option<RgbColor> {
    let value = raw.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Some(hex_to_rgb(hex));
        }
        if hex.len() == 12 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Some(RgbColor {
                r: parse_osc_hex_channel(&hex[0..4])?,
                g: parse_osc_hex_channel(&hex[4..8])?,
                b: parse_osc_hex_channel(&hex[8..12])?,
            });
        }
        return None;
    }
    let stripped = strip_prefix_ci(value, "rgba:").or_else(|| strip_prefix_ci(value, "rgb:"))?;
    let mut channels = stripped.split('/');
    let red = channels.next()?;
    let green = channels.next()?;
    let blue = channels.next()?;
    if channels.next().is_some() {
        return None;
    }
    Some(RgbColor {
        r: parse_osc_hex_channel(red)?,
        g: parse_osc_hex_channel(green)?,
        b: parse_osc_hex_channel(blue)?,
    })
}

fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() >= prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&value[prefix.len()..])
    } else {
        None
    }
}

/// `hexToRgb`, `terminal-colors.ts:20-26`.
fn hex_to_rgb(hex: &str) -> RgbColor {
    let byte = |index: usize| u8::from_str_radix(&hex[index..index + 2], 16).unwrap_or(0);
    RgbColor {
        r: byte(0),
        g: byte(2),
        b: byte(4),
    }
}

/// `parseOscHexChannel`, `terminal-colors.ts:28-36`. Channels carry their own
/// bit depth: `ff` is 8 bits, `ffff` is 16, `f` is 4.
fn parse_osc_hex_channel(channel: &str) -> Option<u8> {
    if channel.is_empty() || !channel.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16u32.checked_pow(channel.len() as u32)? - 1;
    if max == 0 {
        return None;
    }
    let value = u32::from_str_radix(channel, 16).ok()?;
    Some(((value as f64 / max as f64) * 255.0).round() as u8)
}

/// `parseTerminalColorSchemeReport`, `terminal-colors.ts:91-98`.
/// One or more `CSI ?997;1n` (dark) or `CSI ?997;2n` (light), optionally
/// concatenated; the last one wins.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalColorScheme> {
    let mut scheme = None;
    let mut rest = data;
    while let Some(stripped) = rest.strip_prefix("\x1b[?997;") {
        let end = stripped.find('n')?;
        scheme = match &stripped[..end] {
            "2" => Some(TerminalColorScheme::Light),
            "1" => Some(TerminalColorScheme::Dark),
            _ => return None,
        };
        rest = &stripped[end + 1..];
    }
    scheme
}

/// The cell-size report's own pattern, `^\x1b\[6;(\d+);(\d+)t$`
/// (`tui.ts:1182`).
///
/// `consumeCellSizeResponse` answers `true` for a zero-sized report and never
/// for anything else, so "is this a report" and "does it carry usable
/// dimensions" are two questions: the caller eats every report, and only a
/// usable one updates anything.
pub fn is_cell_size_response(data: &str) -> bool {
    let Some(body) = data.strip_prefix("\x1b[6;") else {
        return false;
    };
    let Some(body) = body.strip_suffix('t') else {
        return false;
    };
    let Some((height, width)) = body.split_once(';') else {
        return false;
    };
    !height.is_empty()
        && !width.is_empty()
        && height
            .bytes()
            .chain(width.bytes())
            .all(|byte| byte.is_ascii_digit())
}

/// `consumeCellSizeResponse`'s usable half, `tui.ts:1184-1191`: the reply is
/// `ESC [ 6 ; height ; width t`, and a cell of zero is swallowed.
pub fn parse_cell_size_response(data: &str) -> Option<(u32, u32)> {
    if !is_cell_size_response(data) {
        return None;
    }
    let body = data.strip_prefix("\x1b[6;")?.strip_suffix('t')?;
    let (height, width) = body.split_once(';')?;
    // pi's `parseInt` cannot fail on `\d+`, so a report that stops fitting a
    // `u32` is the same case as a zero cell: consumed, nothing updated.
    let (height_px, width_px) = (height.parse::<u32>().ok()?, width.parse::<u32>().ok()?);
    (height_px > 0 && width_px > 0).then_some((height_px, width_px))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_bytes_match_pi() {
        let query = terminal_color_query();
        assert!(query.starts_with("\x1b]10;?\x07\x1b]11;?\x07"));
        assert!(query.ends_with("\x1b]4;15;?\x07\x1b[c"));
        assert_eq!(query.matches("\x1b]4;").count(), 16);
    }

    #[test]
    fn device_attributes_fence() {
        assert!(is_device_attributes_response("\x1b[?62;c"));
        assert!(is_device_attributes_response("\x1b[?1;2c"));
        // pi's pattern is `\x1b\[\?[\d;]*c`, so a secondary DA reply ends the
        // batch too. Matching that exactly matters more than being cleverer.
        assert!(is_device_attributes_response("\x1b[?62;1c"));
        assert!(!is_device_attributes_response("\x1b[6;20;10t"));
        assert!(!is_device_attributes_response("\x1b[c"), "empty body");
    }

    #[test]
    fn parses_foreground_and_background() {
        assert_eq!(
            parse_osc_color_response("\x1b]10;#ff0000\x07"),
            Some((
                OscColorTarget::Foreground,
                Some(RgbColor { r: 255, g: 0, b: 0 })
            ))
        );
        assert_eq!(
            parse_osc_color_response("\x1b]11;rgb:ffff/0000/0000\x1b\\"),
            Some((
                OscColorTarget::Background,
                Some(RgbColor { r: 255, g: 0, b: 0 })
            ))
        );
    }

    #[test]
    fn parses_palette_entries() {
        assert_eq!(
            parse_osc_color_response("\x1b]4;3;#00ff00\x07"),
            Some((
                OscColorTarget::Palette(3),
                Some(RgbColor { r: 0, g: 255, b: 0 })
            ))
        );
        assert_eq!(
            parse_osc_color_response("\x1b]4;99;#ffffff\x07").map(|t| t.0),
            Some(OscColorTarget::Palette(99))
        );
    }

    #[test]
    fn rejects_unrelated_and_malformed_replies() {
        assert_eq!(parse_osc_color_response("hello"), None);
        assert_eq!(
            parse_osc_color_response("\x1b]10;#ff0000"),
            None,
            "no terminator"
        );
        assert_eq!(parse_osc_color_response("\x1b]12;#ff0000\x07"), None);
        assert_eq!(
            parse_osc_color_response("\x1b]4;#ff0000\x07"),
            None,
            "no index"
        );
        // A reply whose colour does not parse is still a handled reply.
        assert_eq!(
            parse_osc_color_response("\x1b]10;#zz\x07"),
            Some((OscColorTarget::Foreground, None))
        );
    }

    #[test]
    fn colour_value_grammars() {
        assert_eq!(
            parse_osc_color_value("#abc"),
            None,
            "three-digit hex is not a reported form"
        );
        assert_eq!(parse_osc_color_value("#11223344"), None);
        assert_eq!(
            parse_osc_color_value("rgb:ff/80/00"),
            Some(RgbColor {
                r: 255,
                g: 128,
                b: 0
            })
        );
        assert_eq!(
            parse_osc_color_value("rgba:ff/80/00"),
            Some(RgbColor {
                r: 255,
                g: 128,
                b: 0
            })
        );
        // 12-bit hex: three 4-bit channels, each scaled to its own depth.
        assert_eq!(
            parse_osc_color_value("#ffff00000000"),
            Some(RgbColor { r: 255, g: 0, b: 0 })
        );
        assert_eq!(
            parse_osc_color_value("#fff000000f00"),
            Some(RgbColor {
                r: 255,
                g: 0,
                b: 15
            })
        );
        assert_eq!(
            parse_osc_color_value("#fff0000f0000"),
            Some(RgbColor { r: 255, g: 0, b: 0 })
        );
        assert_eq!(
            parse_osc_color_value("#000000000000"),
            Some(RgbColor { r: 0, g: 0, b: 0 })
        );
        // A single hex digit is a 4-bit channel: the maximum is `f`, not `8`.
        assert_eq!(
            parse_osc_color_value("rgb:8/4/2"),
            Some(RgbColor {
                r: 136,
                g: 68,
                b: 34
            })
        );
        assert_eq!(
            parse_osc_color_value("rgb:f/f/f"),
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
    }

    #[test]
    fn colour_scheme_reports() {
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;1n"),
            Some(TerminalColorScheme::Dark)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;2n"),
            Some(TerminalColorScheme::Light)
        );
        assert_eq!(
            parse_terminal_color_scheme_report("\x1b[?997;1n\x1b[?997;2n"),
            Some(TerminalColorScheme::Light),
            "the last report wins"
        );
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;3n"), None);
        assert_eq!(parse_terminal_color_scheme_report("\x1b[?1n"), None);
    }

    #[test]
    fn cell_size_responses() {
        assert_eq!(parse_cell_size_response("\x1b[6;20;10t"), Some((20, 10)));
        assert_eq!(
            parse_cell_size_response("\x1b[6;0;10t"),
            None,
            "zero cells are ignored"
        );
        assert_eq!(parse_cell_size_response("\x1b[6;20;0t"), None);
        assert_eq!(parse_cell_size_response("\x1b[16t"), None);
    }
}
