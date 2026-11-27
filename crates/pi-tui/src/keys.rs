//! Key decoding and matching in pi's key-name vocabulary.
//!
//! Ground truth: `packages/tui/src/keys.ts` (1401 lines). The vocabulary
//! (`escape`, `ctrl+c`, `shift+tab`, `pageUp`, …) is the contract 4b's
//! keybindings and editor depend on, so the port keeps pi's tables and its
//! deliberately raw matching: `matches_key` asks "does this byte sequence
//! produce this key", never "does it parse to an equal name".
//!
//! Kitty protocol state is process-global in pi (`keys.ts:31-39`); it is a
//! process-global `AtomicBool` here for the same reason.

use std::sync::atomic::{AtomicBool, Ordering};

static KITTY_PROTOCOL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// `keys.ts:31-34`.
pub fn set_kitty_protocol_active(active: bool) {
    KITTY_PROTOCOL_ACTIVE.store(active, Ordering::Relaxed);
}

/// `keys.ts:38-40`.
pub fn is_kitty_protocol_active() -> bool {
    KITTY_PROTOCOL_ACTIVE.load(Ordering::Relaxed)
}

/// Modifier bits. `keys.ts:292-299`.
pub mod modifiers {
    /// Shift modifier bit.
    pub const SHIFT: u8 = 1;
    /// Alt modifier bit.
    pub const ALT: u8 = 2;
    /// Ctrl modifier bit.
    pub const CTRL: u8 = 4;
    /// Super modifier bit.
    pub const SUPER: u8 = 8;
    /// Caps Lock + Num Lock.
    pub const LOCK_MASK: u8 = 64 + 128;
    /// The modifier bits a key name can carry; higher bits stay unmatched.
    pub const SUPPORTED: u8 = SHIFT | ALT | CTRL | SUPER;
}

/// Codepoints pi assigns to keys. `keys.ts:301-325`.
pub mod codepoints {
    /// Escape.
    pub const ESCAPE: i64 = 27;
    /// Tab.
    pub const TAB: i64 = 9;
    /// Enter (Return).
    pub const ENTER: i64 = 13;
    /// Space.
    pub const SPACE: i64 = 32;
    /// Backspace.
    pub const BACKSPACE: i64 = 127;
    /// Numpad Enter (Kitty protocol).
    pub const KP_ENTER: i64 = 57414;

    /// Up arrow; non-text keys carry negative codepoints.
    pub const UP: i64 = -1;
    /// Down arrow.
    pub const DOWN: i64 = -2;
    /// Right arrow.
    pub const RIGHT: i64 = -3;
    /// Left arrow.
    pub const LEFT: i64 = -4;

    /// Delete.
    pub const DELETE: i64 = -10;
    /// Insert.
    pub const INSERT: i64 = -11;
    /// Page Up.
    pub const PAGE_UP: i64 = -12;
    /// Page Down.
    pub const PAGE_DOWN: i64 = -13;
    /// Home.
    pub const HOME: i64 = -14;
    /// End.
    pub const END: i64 = -15;
}

/// The 32 symbol keys pi recognizes. `keys.ts:258-290`.
pub const SYMBOL_KEYS: &[char] = &[
    '`', '-', '=', '[', ']', '\\', ';', '\'', ',', '.', '/', '!', '@', '#', '$', '%', '^', '&',
    '*', '(', ')', '_', '+', '|', '~', '{', '}', ':', '<', '>', '?',
];

/// Kitty keypad/function key equivalents. `keys.ts:326-355`.
fn kitty_functional_equivalent(codepoint: i64) -> i64 {
    match codepoint {
        57399..=57408 => 48 + (codepoint - 57399),
        57409 => '.' as i64,
        57410 => '/' as i64,
        57411 => '*' as i64,
        57412 => '-' as i64,
        57413 => '+' as i64,
        57415 => '=' as i64,
        57416 => ',' as i64,
        57417 => codepoints::LEFT,
        57418 => codepoints::RIGHT,
        57419 => codepoints::UP,
        57420 => codepoints::DOWN,
        57421 => codepoints::PAGE_UP,
        57422 => codepoints::PAGE_DOWN,
        57423 => codepoints::HOME,
        57424 => codepoints::END,
        57425 => codepoints::INSERT,
        57426 => codepoints::DELETE,
        other => other,
    }
}

/// `keys.ts:356-358`.
fn normalize_kitty_functional_codepoint(codepoint: i64) -> i64 {
    kitty_functional_equivalent(codepoint)
}

/// Shift+letter is reported with the uppercase codepoint on some terminals;
/// identity matching folds it back. `keys.ts:360-366`.
fn normalize_shifted_letter_identity_codepoint(codepoint: i64, modifier: u8) -> i64 {
    let effective = modifier & !modifiers::LOCK_MASK;
    if effective & modifiers::SHIFT != 0 && (65..=90).contains(&codepoint) {
        codepoint + 32
    } else {
        codepoint
    }
}

/// Plain escape sequences, no modifiers. `keys.ts:368-392`.
fn legacy_key_sequences(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "up" => &["\x1b[A", "\x1bOA"],
        "down" => &["\x1b[B", "\x1bOB"],
        "right" => &["\x1b[C", "\x1bOC"],
        "left" => &["\x1b[D", "\x1bOD"],
        "home" => &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"],
        "end" => &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"],
        "insert" => &["\x1b[2~"],
        "delete" => &["\x1b[3~"],
        "pageUp" => &["\x1b[5~", "\x1b[[5~"],
        "pageDown" => &["\x1b[6~", "\x1b[[6~"],
        "clear" => &["\x1b[E", "\x1bOE"],
        "f1" => &["\x1bOP", "\x1b[11~", "\x1b[[A"],
        "f2" => &["\x1bOQ", "\x1b[12~", "\x1b[[B"],
        "f3" => &["\x1bOR", "\x1b[13~", "\x1b[[C"],
        "f4" => &["\x1bOS", "\x1b[14~", "\x1b[[D"],
        "f5" => &["\x1b[15~", "\x1b[[E"],
        "f6" => &["\x1b[17~"],
        "f7" => &["\x1b[18~"],
        "f8" => &["\x1b[19~"],
        "f9" => &["\x1b[20~"],
        "f10" => &["\x1b[21~"],
        "f11" => &["\x1b[23~"],
        "f12" => &["\x1b[24~"],
        _ => return None,
    })
}

/// Shift-modified legacy sequences. `keys.ts:394-406`.
fn legacy_shift_sequences(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "up" => &["\x1b[a"],
        "down" => &["\x1b[b"],
        "right" => &["\x1b[c"],
        "left" => &["\x1b[d"],
        "clear" => &["\x1b[e"],
        "insert" => &["\x1b[2$"],
        "delete" => &["\x1b[3$"],
        "pageUp" => &["\x1b[5$"],
        "pageDown" => &["\x1b[6$"],
        "home" => &["\x1b[7$"],
        "end" => &["\x1b[8$"],
        _ => return None,
    })
}

/// Ctrl-modified legacy sequences. `keys.ts:408-420`.
fn legacy_ctrl_sequences(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "up" => &["\x1bOa"],
        "down" => &["\x1bOb"],
        "right" => &["\x1bOc"],
        "left" => &["\x1bOd"],
        "clear" => &["\x1bOe"],
        "insert" => &["\x1b[2^"],
        "delete" => &["\x1b[3^"],
        "pageUp" => &["\x1b[5^"],
        "pageDown" => &["\x1b[6^"],
        "home" => &["\x1b[7^"],
        "end" => &["\x1b[8^"],
        _ => return None,
    })
}

/// `keys.ts:485-485`.
fn matches_legacy_sequence(data: &str, sequences: &[&str]) -> bool {
    sequences.contains(&data)
}

/// `keys.ts:487-494`.
fn matches_legacy_modifier_sequence(data: &str, key: &str, modifier: u8) -> bool {
    if modifier == modifiers::SHIFT {
        return legacy_shift_sequences(key)
            .is_some_and(|sequences| matches_legacy_sequence(data, sequences));
    }
    if modifier == modifiers::CTRL {
        return legacy_ctrl_sequences(key)
            .is_some_and(|sequences| matches_legacy_sequence(data, sequences));
    }
    false
}

/// Ambiguous legacy sequences and the key they name. `keys.ts:422-483`.
// Upstream lists each legacy sequence on its own arm, so several sequences
// (SS3, `CSI ~` and `CSI [[` forms of f1-f5) intentionally share a key id.
#[allow(clippy::match_same_arms)]
fn legacy_sequence_key_id(data: &str) -> Option<&'static str> {
    Some(match data {
        "\x1bOA" => "up",
        "\x1bOB" => "down",
        "\x1bOC" => "right",
        "\x1bOD" => "left",
        "\x1bOH" => "home",
        "\x1bOF" => "end",
        "\x1b[E" | "\x1bOE" => "clear",
        "\x1bOe" => "ctrl+clear",
        "\x1b[e" => "shift+clear",
        "\x1b[2~" => "insert",
        "\x1b[2$" => "shift+insert",
        "\x1b[2^" => "ctrl+insert",
        "\x1b[3$" => "shift+delete",
        "\x1b[3^" => "ctrl+delete",
        "\x1b[[5~" => "pageUp",
        "\x1b[[6~" => "pageDown",
        "\x1b[a" => "shift+up",
        "\x1b[b" => "shift+down",
        "\x1b[c" => "shift+right",
        "\x1b[d" => "shift+left",
        "\x1bOa" => "ctrl+up",
        "\x1bOb" => "ctrl+down",
        "\x1bOc" => "ctrl+right",
        "\x1bOd" => "ctrl+left",
        "\x1b[5$" => "shift+pageUp",
        "\x1b[6$" => "shift+pageDown",
        "\x1b[7$" => "shift+home",
        "\x1b[8$" => "shift+end",
        "\x1b[5^" => "ctrl+pageUp",
        "\x1b[6^" => "ctrl+pageDown",
        "\x1b[7^" => "ctrl+home",
        "\x1b[8^" => "ctrl+end",
        "\x1bOP" => "f1",
        "\x1bOQ" => "f2",
        "\x1bOR" => "f3",
        "\x1bOS" => "f4",
        "\x1b[11~" => "f1",
        "\x1b[12~" => "f2",
        "\x1b[13~" => "f3",
        "\x1b[14~" => "f4",
        "\x1b[[A" => "f1",
        "\x1b[[B" => "f2",
        "\x1b[[C" => "f3",
        "\x1b[[D" => "f4",
        "\x1b[[E" => "f5",
        "\x1b[15~" => "f5",
        "\x1b[17~" => "f6",
        "\x1b[18~" => "f7",
        "\x1b[19~" => "f8",
        "\x1b[20~" => "f9",
        "\x1b[21~" => "f10",
        "\x1b[23~" => "f11",
        "\x1b[24~" => "f12",
        "\x1bb" => "alt+left",
        "\x1bf" => "alt+right",
        "\x1bp" => "alt+up",
        "\x1bn" => "alt+down",
        _ => return None,
    })
}

/// Kitty protocol key event type (flag 2). `keys.ts:505`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventType {
    Press,
    Repeat,
    Release,
}

/// Parsed Kitty CSI sequence. `keys.ts:511-518`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KittySequence {
    /// The key's primary codepoint; negative for arrows and functional keys.
    pub codepoint: i64,
    /// Shifted version of the key (when shift is pressed).
    pub shifted_key: Option<i64>,
    /// Key in the standard PC-101 layout (for non-Latin layouts).
    pub base_layout_key: Option<i64>,
    /// Modifier bits (see [`modifiers`]); the 1-indexed wire value is already decoded.
    pub modifier: u8,
    /// Press, repeat or release; the latter two need Kitty's event-type flag.
    pub event_type: KeyEventType,
}

/// xterm modifyOtherKeys sequence. `keys.ts:520-523`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModifyOtherKeysSequence {
    /// The key's codepoint.
    pub codepoint: i64,
    /// Modifier bits, already 0-indexed.
    pub modifier: u8,
}

/// `keys.ts:579-585`.
fn parse_event_type(value: Option<&str>) -> KeyEventType {
    match value.and_then(|value| value.parse::<u32>().ok()) {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

/// Split the modifier / event part of a Kitty sequence body: `2`, `5:3`.
fn split_modifier_event(body: &str) -> (Option<&str>, Option<&str>) {
    match body.split_once(':') {
        Some((modifier, event)) => (Some(modifier), Some(event)),
        None => (Some(body), None),
    }
}

/// `keys.ts:587-651`. Handles `\x1b[<cp>[:<shifted>[:<base>]];<mod>[:<event>]u`,
/// modified arrows (`\x1b[1;<mod>[:<event>]A-D`), modified functional keys
/// (`\x1b[<n>;<mod>[:<event>]~`) and modified home/end (`\x1b[1;<mod>[HF]`).
pub fn parse_kitty_sequence(data: &str) -> Option<KittySequence> {
    let rest = data.strip_prefix("\x1b[")?;
    let final_byte = rest.chars().last()?;
    let body = &rest[..rest.len() - final_byte.len_utf8()];
    match final_byte {
        // `\x1b[<n>;<mod>[:<event>]~`
        '~' => {
            let (key_text, modifiers) = body.split_once(';').unwrap_or((body, ""));
            if key_text.is_empty() || !key_text.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let functional = match key_text.parse::<i64>().ok()? {
                2 => codepoints::INSERT,
                3 => codepoints::DELETE,
                5 => codepoints::PAGE_UP,
                6 => codepoints::PAGE_DOWN,
                7 => codepoints::HOME,
                8 => codepoints::END,
                _ => return None,
            };
            let (modifier_text, event_text) = split_modifier_event(modifiers);
            Some(KittySequence {
                codepoint: functional,
                shifted_key: None,
                base_layout_key: None,
                modifier: kitty_modifier(modifier_text),
                event_type: parse_event_type(event_text.filter(|s| !s.is_empty())),
            })
        }
        // `\x1b[1;<mod>[:<event>]{A,B,C,D,H,F}`
        'A' | 'B' | 'C' | 'D' | 'H' | 'F' => {
            let codepoint = match final_byte {
                'A' => codepoints::UP,
                'B' => codepoints::DOWN,
                'C' => codepoints::RIGHT,
                'D' => codepoints::LEFT,
                'H' => codepoints::HOME,
                _ => codepoints::END,
            };
            let (modifier_text, event_text) = body
                .strip_prefix("1;")
                .map_or((None, None), split_modifier_event);
            Some(KittySequence {
                codepoint,
                shifted_key: None,
                base_layout_key: None,
                modifier: kitty_modifier(modifier_text),
                event_type: parse_event_type(event_text.filter(|s| !s.is_empty())),
            })
        }
        // `\x1b[<cp>[:<shifted>[:<base>]];<mod>[:<event>]u`, plus the bare
        // `\x1b[<cp>u` form.
        'u' => {
            let (codepoint_part, modifiers) = match body.split_once(';') {
                Some((codepoint, modifiers)) => (codepoint, modifiers),
                None => (body, ""),
            };
            let mut fields = codepoint_part.split(':');
            let codepoint_text = fields.next().unwrap_or("");
            if codepoint_text.is_empty() || !codepoint_text.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let shifted_key = fields
                .next()
                .filter(|s| !s.is_empty())
                .and_then(|s| s.parse().ok());
            let base_layout_key = fields.next().and_then(|s| s.parse().ok());
            let (modifier_text, event_text) = split_modifier_event(modifiers);
            Some(KittySequence {
                codepoint: codepoint_text.parse().ok()?,
                shifted_key,
                base_layout_key,
                modifier: kitty_modifier(modifier_text),
                event_type: parse_event_type(event_text.filter(|s| !s.is_empty())),
            })
        }
        _ => None,
    }
}

/// Kitty reports modifiers 1-indexed; an absent field means "no modifiers".
fn kitty_modifier(text: Option<&str>) -> u8 {
    match text.filter(|value| !value.is_empty()) {
        Some(value) => match value.parse::<i64>() {
            Ok(value) if value >= 1 => (value - 1) as u8,
            _ => 0,
        },
        None => 0,
    }
}

/// `keys.ts:653-694`.
fn matches_kitty_sequence(data: &str, expected_codepoint: i64, expected_modifier: u8) -> bool {
    let Some(parsed) = parse_kitty_sequence(data) else {
        return false;
    };
    let actual_mod = parsed.modifier & !modifiers::LOCK_MASK;
    let expected_mod = expected_modifier & !modifiers::LOCK_MASK;
    if actual_mod != expected_mod {
        return false;
    }
    let normalized = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(parsed.codepoint),
        parsed.modifier,
    );
    let normalized_expected = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(expected_codepoint),
        expected_modifier,
    );
    if normalized == normalized_expected {
        return true;
    }
    // Base-layout fallback: only for codepoints that are not already a Latin
    // letter or a known symbol, so remapped layouts cannot alias.
    if let Some(base) = parsed.base_layout_key
        && base == expected_codepoint
    {
        let is_latin_letter = (97..=122).contains(&normalized);
        let is_known_symbol =
            char::from_u32(normalized as u32).is_some_and(|c| SYMBOL_KEYS.contains(&c));
        if !is_latin_letter && !is_known_symbol {
            return true;
        }
    }
    false
}

/// `keys.ts:696-707`.
pub fn parse_modify_other_keys_sequence(data: &str) -> Option<ModifyOtherKeysSequence> {
    let body = data.strip_prefix("\x1b[27;")?.strip_suffix('~')?;
    let (mod_text, codepoint_text) = body.split_once(';')?;
    if !mod_text.bytes().all(|b| b.is_ascii_digit())
        || !codepoint_text.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(ModifyOtherKeysSequence {
        codepoint: codepoint_text.parse().ok()?,
        modifier: (mod_text.parse::<i64>().ok()? - 1) as u8,
    })
}

/// `keys.ts:709-713`.
fn matches_modify_other_keys(data: &str, expected_keycode: i64, expected_modifier: u8) -> bool {
    parse_modify_other_keys_sequence(data).is_some_and(|parsed| {
        parsed.codepoint == expected_keycode && parsed.modifier == expected_modifier
    })
}

/// `keys.ts:766-774`.
fn matches_printable_modify_other_keys(
    data: &str,
    expected_keycode: i64,
    expected_modifier: u8,
) -> bool {
    if expected_modifier == 0 {
        return false;
    }
    let Some(parsed) = parse_modify_other_keys_sequence(data) else {
        return false;
    };
    if parsed.modifier != expected_modifier {
        return false;
    }
    normalize_shifted_letter_identity_codepoint(parsed.codepoint, parsed.modifier)
        == normalize_shifted_letter_identity_codepoint(expected_keycode, expected_modifier)
}

/// `keys.ts:715-728`.
fn is_windows_terminal_session() -> bool {
    fn set(name: &str) -> bool {
        std::env::var_os(name).is_some_and(|value| !value.is_empty())
    }
    set("WT_SESSION") && !set("SSH_CONNECTION") && !set("SSH_CLIENT") && !set("SSH_TTY")
}

/// `keys.ts:730-747`. Raw 0x08 is ambiguous: Windows Terminal means
/// Ctrl+Backspace, most other terminals mean plain Backspace.
fn matches_raw_backspace(data: &str, expected_modifier: u8) -> bool {
    if data == "\x7f" {
        return expected_modifier == 0;
    }
    if data != "\x08" {
        return false;
    }
    if is_windows_terminal_session() {
        expected_modifier == modifiers::CTRL
    } else {
        expected_modifier == 0
    }
}

/// `keys.ts:749-760`.
fn raw_ctrl_char(key: char) -> Option<char> {
    let lower = key.to_ascii_lowercase();
    let code = lower as u32;
    let is_letter = (97..=122).contains(&code);
    if is_letter || matches!(lower, '[' | '\\' | ']' | '_' | '-') {
        // `-` shares a physical key with `_` on US keyboards.
        let code = if lower == '-' { '_' } else { lower } as u32;
        return char::from_u32(code & 0x1f);
    }
    None
}

/// `keys.ts:776-786`.
fn format_key_name_with_modifiers(key_name: &str, modifier: u8) -> Option<String> {
    let effective = modifier & !modifiers::LOCK_MASK;
    if effective & !modifiers::SUPPORTED != 0 {
        return None;
    }
    let mut mods: Vec<&str> = Vec::new();
    if effective & modifiers::SHIFT != 0 {
        mods.push("shift");
    }
    if effective & modifiers::CTRL != 0 {
        mods.push("ctrl");
    }
    if effective & modifiers::ALT != 0 {
        mods.push("alt");
    }
    if effective & modifiers::SUPER != 0 {
        mods.push("super");
    }
    Some(if mods.is_empty() {
        key_name.to_string()
    } else {
        format!("{}+{}", mods.join("+"), key_name)
    })
}

/// Split `ctrl+shift+p` into its key and modifier bits. `keys.ts:788-805`.
fn parse_key_id(key_id: &str) -> Option<(String, u8)> {
    let lower = key_id.to_lowercase();
    let mut parts: Vec<&str> = lower.split('+').collect();
    let key = parts.pop()?;
    if key.is_empty() {
        return None;
    }
    let mut modifier = 0u8;
    for part in &parts {
        match *part {
            "ctrl" => modifier |= modifiers::CTRL,
            "shift" => modifier |= modifiers::SHIFT,
            "alt" => modifier |= modifiers::ALT,
            "super" => modifier |= modifiers::SUPER,
            _ => {}
        }
    }
    Some((key.to_string(), modifier))
}

/// Key ids are lowercased before matching (`keys.ts:790`), but pi's legacy
/// tables keep the camelCase names.
fn legacy_key_name(key: &str) -> String {
    match key {
        "pageup" => "pageUp".to_string(),
        "pagedown" => "pageDown".to_string(),
        other => other.to_string(),
    }
}

/// `keys.ts:820-1210`. Matches raw input against a key identifier.
///
/// Supported identifiers: single keys (`escape`, `tab`, `enter`, `backspace`,
/// `delete`, `home`, `end`, `space`, arrows, `f1`..`f12`), any modifier
/// prefix in any order (`ctrl+c`, `shift+tab`, `ctrl+alt+x`, `super+k`), and
/// printable keys (`a`-`z`, `0`-`9`, symbols).
// One arm per upstream `keys.ts:820-1210` branch; splitting it would lose the
// 1:1 correspondence with the reference matcher.
#[allow(clippy::too_many_lines)]
pub fn matches_key(data: &str, key_id: &str) -> bool {
    let Some((key, modifier)) = parse_key_id(key_id) else {
        return false;
    };
    let kitty = is_kitty_protocol_active();

    match key.as_str() {
        "escape" | "esc" => {
            modifier == 0
                && (data == "\x1b"
                    || matches_kitty_sequence(data, codepoints::ESCAPE, 0)
                    || matches_modify_other_keys(data, codepoints::ESCAPE, 0))
        }
        "space" => {
            if !kitty {
                if modifier == modifiers::CTRL && data == "\x00" {
                    return true;
                }
                if modifier == modifiers::ALT && data == "\x1b " {
                    return true;
                }
            }
            if modifier == 0 {
                data == " "
                    || matches_kitty_sequence(data, codepoints::SPACE, 0)
                    || matches_modify_other_keys(data, codepoints::SPACE, 0)
            } else {
                matches_kitty_sequence(data, codepoints::SPACE, modifier)
                    || matches_modify_other_keys(data, codepoints::SPACE, modifier)
            }
        }
        "tab" => {
            if modifier == modifiers::SHIFT {
                return data == "\x1b[Z"
                    || matches_kitty_sequence(data, codepoints::TAB, modifiers::SHIFT)
                    || matches_modify_other_keys(data, codepoints::TAB, modifiers::SHIFT);
            }
            if modifier == 0 {
                return data == "\t" || matches_kitty_sequence(data, codepoints::TAB, 0);
            }
            matches_kitty_sequence(data, codepoints::TAB, modifier)
                || matches_modify_other_keys(data, codepoints::TAB, modifier)
        }
        "enter" | "return" => match modifier {
            modifiers::SHIFT => {
                if matches_kitty_sequence(data, codepoints::ENTER, modifiers::SHIFT)
                    || matches_kitty_sequence(data, codepoints::KP_ENTER, modifiers::SHIFT)
                {
                    return true;
                }
                if matches_modify_other_keys(data, codepoints::ENTER, modifiers::SHIFT) {
                    return true;
                }
                // With Kitty active these are custom terminal mappings:
                // `\x1b\r` is Kitty's shift+enter, `\n` is Ghostty's.
                kitty && (data == "\x1b\r" || data == "\n")
            }
            modifiers::ALT => {
                if matches_kitty_sequence(data, codepoints::ENTER, modifiers::ALT)
                    || matches_kitty_sequence(data, codepoints::KP_ENTER, modifiers::ALT)
                {
                    return true;
                }
                if matches_modify_other_keys(data, codepoints::ENTER, modifiers::ALT) {
                    return true;
                }
                !kitty && data == "\x1b\r"
            }
            0 => {
                data == "\r"
                    || (!kitty && data == "\n")
                    || data == "\x1bOM"
                    || matches_kitty_sequence(data, codepoints::ENTER, 0)
                    || matches_kitty_sequence(data, codepoints::KP_ENTER, 0)
            }
            _ => {
                matches_kitty_sequence(data, codepoints::ENTER, modifier)
                    || matches_kitty_sequence(data, codepoints::KP_ENTER, modifier)
                    || matches_modify_other_keys(data, codepoints::ENTER, modifier)
            }
        },
        "backspace" => match modifier {
            modifiers::ALT => {
                data == "\x1b\x7f"
                    || data == "\x1b\u{8}"
                    || matches_kitty_sequence(data, codepoints::BACKSPACE, modifiers::ALT)
                    || matches_modify_other_keys(data, codepoints::BACKSPACE, modifiers::ALT)
            }
            modifiers::CTRL => {
                matches_raw_backspace(data, modifiers::CTRL)
                    || matches_kitty_sequence(data, codepoints::BACKSPACE, modifiers::CTRL)
                    || matches_modify_other_keys(data, codepoints::BACKSPACE, modifiers::CTRL)
            }
            0 => {
                matches_raw_backspace(data, 0)
                    || matches_kitty_sequence(data, codepoints::BACKSPACE, 0)
                    || matches_modify_other_keys(data, codepoints::BACKSPACE, 0)
            }
            _ => {
                matches_kitty_sequence(data, codepoints::BACKSPACE, modifier)
                    || matches_modify_other_keys(data, codepoints::BACKSPACE, modifier)
            }
        },
        // pi lowercases the key id before switching, so the page keys are
        // matched in lower case (`keys.ts:1010-1040`).
        "insert" | "delete" | "home" | "end" | "pageup" | "pagedown" | "clear" => {
            let functional = match key.as_str() {
                "insert" => codepoints::INSERT,
                "delete" => codepoints::DELETE,
                "home" => codepoints::HOME,
                "end" => codepoints::END,
                "pageup" => codepoints::PAGE_UP,
                "pagedown" => codepoints::PAGE_DOWN,
                _ => {
                    // `clear` has no Kitty codepoint in pi.
                    return if modifier == 0 {
                        matches_legacy_sequence(data, legacy_key_sequences("clear").unwrap_or(&[]))
                    } else {
                        matches_legacy_modifier_sequence(data, "clear", modifier)
                    };
                }
            };
            if modifier == 0 {
                return matches_legacy_sequence(
                    data,
                    legacy_key_sequences(&legacy_key_name(&key)).unwrap_or(&[]),
                ) || matches_kitty_sequence(data, functional, 0);
            }
            if matches_legacy_modifier_sequence(data, &legacy_key_name(&key), modifier) {
                return true;
            }
            matches_kitty_sequence(data, functional, modifier)
        }
        "up" | "down" | "left" | "right" => {
            let arrow = match key.as_str() {
                "up" => codepoints::UP,
                "down" => codepoints::DOWN,
                "left" => codepoints::LEFT,
                _ => codepoints::RIGHT,
            };
            if modifier == modifiers::ALT {
                let raw = match key.as_str() {
                    "up" => data == "\x1bp",
                    "down" => data == "\x1bn",
                    "left" => data == "\x1bb" || (!kitty && data == "\x1bB") || data == "\x1b[1;3D",
                    _ => data == "\x1bf" || (!kitty && data == "\x1bF") || data == "\x1b[1;3C",
                };
                return raw || matches_kitty_sequence(data, arrow, modifiers::ALT);
            }
            if modifier == modifiers::CTRL {
                let raw = match key.as_str() {
                    "up" => data == "\x1b[1;5A",
                    "down" => data == "\x1b[1;5B",
                    "left" => data == "\x1b[1;5D",
                    _ => data == "\x1b[1;5C",
                };
                return raw
                    || matches_legacy_modifier_sequence(
                        data,
                        &legacy_key_name(&key),
                        modifiers::CTRL,
                    )
                    || matches_kitty_sequence(data, arrow, modifiers::CTRL);
            }
            if modifier == 0 {
                return matches_legacy_sequence(
                    data,
                    legacy_key_sequences(&legacy_key_name(&key)).unwrap_or(&[]),
                ) || matches_kitty_sequence(data, arrow, 0);
            }
            if matches_legacy_modifier_sequence(data, &legacy_key_name(&key), modifier) {
                return true;
            }
            matches_kitty_sequence(data, arrow, modifier)
        }
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            modifier == 0
                && matches_legacy_sequence(data, legacy_key_sequences(&key).unwrap_or(&[]))
        }
        _ if key.len() == 1 => {
            let Some(base) = key.chars().next() else {
                return false;
            };
            let is_letter = base.is_ascii_lowercase();
            let is_digit = base.is_ascii_digit();
            if !is_letter && !is_digit && !SYMBOL_KEYS.contains(&base) {
                return false;
            }
            let codepoint = base as i64;
            let raw_ctrl = raw_ctrl_char(base);

            if modifier == modifiers::CTRL | modifiers::ALT
                && !kitty
                && let Some(raw) = raw_ctrl
                && data == format!("\x1b{raw}")
            {
                // Legacy ctrl+alt+key is ESC followed by the control character.
                return true;
            }
            // tmux CSI-u / modifyOtherKeys must still be recognized.
            if modifier == modifiers::ALT && !kitty && data == format!("\x1b{base}") {
                return true;
            }
            if modifier == modifiers::CTRL {
                if let Some(raw) = raw_ctrl
                    && data == raw.to_string()
                {
                    return true;
                }
                return matches_kitty_sequence(data, codepoint, modifiers::CTRL)
                    || matches_printable_modify_other_keys(data, codepoint, modifiers::CTRL);
            }
            if modifier == modifiers::SHIFT | modifiers::CTRL {
                return matches_kitty_sequence(data, codepoint, modifier)
                    || matches_printable_modify_other_keys(data, codepoint, modifier);
            }
            if modifier == modifiers::SHIFT {
                if is_letter && data == base.to_uppercase().to_string() {
                    return true;
                }
                return matches_kitty_sequence(data, codepoint, modifiers::SHIFT)
                    || matches_printable_modify_other_keys(data, codepoint, modifiers::SHIFT);
            }
            if modifier != 0 {
                return matches_kitty_sequence(data, codepoint, modifier)
                    || matches_printable_modify_other_keys(data, codepoint, modifier);
            }
            // Plain key: the raw character, or its Kitty press/release form.
            data == base.to_string() || matches_kitty_sequence(data, codepoint, 0)
        }
        _ => false,
    }
}

/// `keys.ts:1212-1249`.
fn format_parsed_key(codepoint: i64, modifier: u8, base_layout_key: Option<i64>) -> Option<String> {
    let normalized = normalize_kitty_functional_codepoint(codepoint);
    let identity = normalize_shifted_letter_identity_codepoint(normalized, modifier);

    // Use the base-layout key only when the codepoint is not a recognized
    // Latin letter, digit or symbol: a remapped layout must not rename keys.
    let is_latin_letter = (97..=122).contains(&identity);
    let is_digit = (48..=57).contains(&identity);
    let is_known_symbol = char::from_u32(identity as u32).is_some_and(|c| SYMBOL_KEYS.contains(&c));
    let effective = if is_latin_letter || is_digit || is_known_symbol {
        identity
    } else {
        base_layout_key.unwrap_or(identity)
    };

    let key_name = if effective == codepoints::ESCAPE {
        "escape"
    } else if effective == codepoints::TAB {
        "tab"
    } else if effective == codepoints::ENTER || effective == codepoints::KP_ENTER {
        "enter"
    } else if effective == codepoints::SPACE {
        "space"
    } else if effective == codepoints::BACKSPACE {
        "backspace"
    } else if effective == codepoints::DELETE {
        "delete"
    } else if effective == codepoints::INSERT {
        "insert"
    } else if effective == codepoints::HOME {
        "home"
    } else if effective == codepoints::END {
        "end"
    } else if effective == codepoints::PAGE_UP {
        "pageUp"
    } else if effective == codepoints::PAGE_DOWN {
        "pageDown"
    } else if effective == codepoints::UP {
        "up"
    } else if effective == codepoints::DOWN {
        "down"
    } else if effective == codepoints::LEFT {
        "left"
    } else if effective == codepoints::RIGHT {
        "right"
    } else if (48..=57).contains(&effective)
        || (97..=122).contains(&effective)
        || char::from_u32(effective as u32).is_some_and(|c| SYMBOL_KEYS.contains(&c))
    {
        char::from_u32(effective as u32)?.to_string().leak()
    } else {
        return None;
    };
    format_key_name_with_modifiers(key_name, modifier)
}

/// `keys.ts:1251-1331`. Raw input data to a pi key identifier.
// Mirrors the upstream decoder's linear fallback order (kitty, MOK, legacy);
// splitting it would obscure that parity.
#[allow(clippy::too_many_lines)]
pub fn parse_key(data: &str) -> Option<String> {
    if let Some(kitty) = parse_kitty_sequence(data) {
        return format_parsed_key(kitty.codepoint, kitty.modifier, kitty.base_layout_key);
    }
    if let Some(modify_other_keys) = parse_modify_other_keys_sequence(data) {
        return format_parsed_key(
            modify_other_keys.codepoint,
            modify_other_keys.modifier,
            None,
        );
    }

    // Mode-aware legacy ambiguity (`keys.ts:1264-1269`).
    if is_kitty_protocol_active() && (data == "\x1b\r" || data == "\n") {
        return Some("shift+enter".to_string());
    }

    if let Some(key_id) = legacy_sequence_key_id(data) {
        return Some(key_id.to_string());
    }

    let kitty = is_kitty_protocol_active();
    if data == "\x1b" {
        return Some("escape".to_string());
    }
    if data == "\x1c" {
        return Some("ctrl+\\".to_string());
    }
    if data == "\x1d" {
        return Some("ctrl+]".to_string());
    }
    if data == "\x1f" {
        return Some("ctrl+-".to_string());
    }
    if data == "\x1b\x1b" {
        return Some("ctrl+alt+[".to_string());
    }
    if data == "\x1b\x1c" {
        return Some("ctrl+alt+\\".to_string());
    }
    if data == "\x1b\x1d" {
        return Some("ctrl+alt+]".to_string());
    }
    if data == "\x1b\x1f" {
        return Some("ctrl+alt+-".to_string());
    }
    if data == "\t" {
        return Some("tab".to_string());
    }
    if data == "\r" || (!kitty && data == "\n") || data == "\x1bOM" {
        return Some("enter".to_string());
    }
    if data == "\x00" {
        return Some("ctrl+space".to_string());
    }
    if data == " " {
        return Some("space".to_string());
    }
    if data == "\x7f" {
        return Some("backspace".to_string());
    }
    if data == "\x08" {
        return Some(
            if is_windows_terminal_session() {
                "ctrl+backspace"
            } else {
                "backspace"
            }
            .to_string(),
        );
    }
    if data == "\x1b[Z" {
        return Some("shift+tab".to_string());
    }
    if !kitty && data == "\x1b\r" {
        return Some("alt+enter".to_string());
    }
    if !kitty && data == "\x1b " {
        return Some("alt+space".to_string());
    }
    if data == "\x1b\x7f" || data == "\x1b\u{8}" {
        return Some("alt+backspace".to_string());
    }
    if !kitty && data == "\x1bB" {
        return Some("alt+left".to_string());
    }
    if !kitty && data == "\x1bF" {
        return Some("alt+right".to_string());
    }
    if !kitty && data.chars().count() == 2 && data.starts_with('\x1b') {
        let code = data[1..].chars().next().map(|ch| ch as u32)?;
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+alt+{}", char::from_u32(code + 96)?));
        }
        let key = char::from_u32(code)?;
        if (97..=122).contains(&code) || (48..=57).contains(&code) || SYMBOL_KEYS.contains(&key) {
            return Some(format!("alt+{key}"));
        }
    }
    if data == "\x1b[A" {
        return Some("up".to_string());
    }
    if data == "\x1b[B" {
        return Some("down".to_string());
    }
    if data == "\x1b[C" {
        return Some("right".to_string());
    }
    if data == "\x1b[D" {
        return Some("left".to_string());
    }
    if data == "\x1b[H" || data == "\x1bOH" {
        return Some("home".to_string());
    }
    if data == "\x1b[F" || data == "\x1bOF" {
        return Some("end".to_string());
    }
    if data == "\x1b[3~" {
        return Some("delete".to_string());
    }
    if data == "\x1b[5~" {
        return Some("pageUp".to_string());
    }
    if data == "\x1b[6~" {
        return Some("pageDown".to_string());
    }

    if data.chars().count() == 1 {
        let ch = data.chars().next()?;
        let code = ch as u32;
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+{}", char::from_u32(code + 96)?));
        }
        if (32..=126).contains(&code) {
            return Some(ch.to_string());
        }
    }
    None
}

/// `keys.ts:527-555`. Only meaningful with the Kitty protocol (flag 2) on.
pub fn is_key_release(data: &str) -> bool {
    // Bracketed paste content may contain ":3F" (a MAC address), so a paste is
    // never a release.
    if data.contains("\x1b[200~") {
        return false;
    }
    [":3u", ":3~", ":3A", ":3B", ":3C", ":3D", ":3H", ":3F"]
        .iter()
        .any(|needle| data.contains(needle))
}

/// `keys.ts:557-577`.
pub fn is_key_repeat(data: &str) -> bool {
    if data.contains("\x1b[200~") {
        return false;
    }
    [":2u", ":2~", ":2A", ":2B", ":2C", ":2D", ":2H", ":2F"]
        .iter()
        .any(|needle| data.contains(needle))
}

/// `keys.ts:1333-1383`. Kitty CSI-u printable decoding: only plain or
/// shift-modified text keys are accepted, so modifier-only events never
/// insert characters.
pub fn decode_kitty_printable(data: &str) -> Option<char> {
    const ALLOWED: u8 = modifiers::SHIFT | modifiers::LOCK_MASK;
    let parsed = parse_kitty_sequence(data)?;
    if !data.ends_with('u') || !data.starts_with("\x1b[") || data[2..].contains('~') {
        return None;
    }
    let modifier = parsed.modifier;
    if modifier & !ALLOWED != 0 {
        return None;
    }
    if modifier & (modifiers::ALT | modifiers::CTRL) != 0 {
        return None;
    }
    let mut effective = parsed.codepoint;
    if modifier & modifiers::SHIFT != 0
        && let Some(shifted) = parsed.shifted_key
    {
        effective = shifted;
    }
    effective = normalize_kitty_functional_codepoint(effective);
    if effective < 32 {
        return None;
    }
    char::from_u32(effective as u32)
}

/// `keys.ts:1385-1397`.
fn decode_modify_other_keys_printable(data: &str) -> Option<char> {
    let parsed = parse_modify_other_keys_sequence(data)?;
    let modifier = parsed.modifier & !modifiers::LOCK_MASK;
    if modifier & !modifiers::SHIFT != 0 {
        return None;
    }
    if parsed.codepoint < 32 {
        return None;
    }
    char::from_u32(parsed.codepoint as u32)
}

/// `keys.ts:1399-1401`.
pub fn decode_printable_key(data: &str) -> Option<char> {
    decode_kitty_printable(data).or_else(|| decode_modify_other_keys_printable(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the Kitty global so tests do not race under
    /// `RUST_TEST_THREADS=4`.
    static KITTY_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_kitty(active: bool, body: impl FnOnce()) {
        let _guard = KITTY_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_kitty_protocol_active(active);
        body();
        set_kitty_protocol_active(false);
    }

    #[test]
    fn parses_legacy_sequences() {
        for (data, expected) in [
            ("\x1b[A", "up"),
            ("\x1bOB", "down"),
            ("\x1b[3~", "delete"),
            ("\x1b[Z", "shift+tab"),
            ("\x1b[5~", "pageUp"),
            ("\x1b[[6~", "pageDown"),
            ("\r", "enter"),
            ("\t", "tab"),
            ("\x7f", "backspace"),
            (" ", "space"),
            ("\x1b", "escape"),
            ("\x03", "ctrl+c"),
            ("\x1b\x1b", "ctrl+alt+["),
            ("a", "a"),
            ("/", "/"),
        ] {
            assert_eq!(parse_key(data).as_deref(), Some(expected), "data {data:?}");
        }
    }

    #[test]
    fn parses_kitty_csi_u_sequences() {
        assert_eq!(
            parse_key("\x1b[97;5u").as_deref(),
            Some("ctrl+a"),
            "modifier field is 1-indexed"
        );
        assert_eq!(parse_key("\x1b[97u").as_deref(), Some("a"));
        assert_eq!(parse_key("\x1b[13;2u").as_deref(), Some("shift+enter"));
        assert_eq!(parse_key("\x1b[57414;1u").as_deref(), Some("enter"));
        assert_eq!(parse_key("\x1b[1;5A").as_deref(), Some("ctrl+up"));
        assert_eq!(parse_key("\x1b[3;5~").as_deref(), Some("ctrl+delete"));
        assert_eq!(parse_key("\x1b[1;2H").as_deref(), Some("shift+home"));
        // Shift+letter identity: CSI-u may report the uppercase codepoint.
        assert_eq!(parse_key("\x1b[65;2u").as_deref(), Some("shift+a"));
    }

    #[test]
    fn parses_modify_other_keys_sequences() {
        assert_eq!(parse_key("\x1b[27;5;97~").as_deref(), Some("ctrl+a"));
        assert_eq!(parse_key("\x1b[27;1;13~").as_deref(), Some("enter"));
    }

    #[test]
    fn kitty_mode_resolves_ambiguous_sequences() {
        with_kitty(false, || {
            assert_eq!(parse_key("\n").as_deref(), Some("enter"));
            assert_eq!(parse_key("\x1b\r").as_deref(), Some("alt+enter"));
            assert_eq!(parse_key("\x1b ").as_deref(), Some("alt+space"));
        });
        with_kitty(true, || {
            assert_eq!(parse_key("\n").as_deref(), Some("shift+enter"));
            assert_eq!(parse_key("\x1b\r").as_deref(), Some("shift+enter"));
        });
    }

    #[test]
    fn matches_special_keys() {
        assert!(matches_key("\x1b", "escape"));
        assert!(matches_key("\x1b", "esc"));
        assert!(matches_key("\r", "enter"));
        assert!(matches_key("\r", "return"));
        assert!(matches_key("\t", "tab"));
        assert!(matches_key("\x1b[Z", "shift+tab"));
        assert!(matches_key("\x7f", "backspace"));
        assert!(matches_key(" ", "space"));
        assert!(matches_key("\x1b[A", "up"));
        assert!(matches_key("\x1bOD", "left"));
        assert!(matches_key("\x1b[3~", "delete"));
        assert!(matches_key("\x1b[5~", "pageUp"));
        assert!(matches_key("\x1b[6~", "pageDown"));
        assert!(matches_key("\x1b[5;5~", "ctrl+pageUp"));
        assert!(matches_key("\x1b[6;5~", "ctrl+pageDown"));
        assert!(matches_key("\x1bOP", "f1"));
        assert!(!matches_key("\x1b[1;5A", "enter"));
        assert!(!matches_key("\x1b[Z", "tab"));
    }

    #[test]
    fn matches_modified_arrows() {
        assert!(matches_key("\x1b[1;5D", "ctrl+left"));
        assert!(matches_key("\x1bOd", "ctrl+left"));
        assert!(matches_key("\x1b[a", "shift+up"));
        assert!(matches_key("\x1bb", "alt+left"));
        assert!(matches_key("\x1bOa", "ctrl+up"));
    }

    #[test]
    fn matches_printable_keys() {
        assert!(matches_key("a", "a"));
        assert!(matches_key("\x1b[97u", "a"));
        assert!(matches_key("\x01", "ctrl+a"));
        assert!(matches_key("\x1b[97;5u", "ctrl+a"));
        assert!(matches_key("\x1b[27;5;97~", "ctrl+a"));
        assert!(matches_key("A", "shift+a"));
        assert!(matches_key("\x1ba", "alt+a"));
        assert!(matches_key("\x1b\x01", "ctrl+alt+a"));
        assert!(matches_key("/", "/"));
        assert!(!matches_key("b", "a"));
        assert!(!matches_key("\x01", "ctrl+b"));
    }

    #[test]
    fn modifier_order_does_not_matter() {
        assert!(matches_key("\x1b[97;6u", "ctrl+shift+a"));
        assert!(matches_key("\x1b[97;6u", "shift+ctrl+a"));
    }

    #[test]
    fn release_and_repeat_probes() {
        assert!(is_key_release("\x1b[97;1:3u"));
        assert!(is_key_repeat("\x1b[97;1:2u"));
        assert!(!is_key_release("\x1b[97;1u"));
        assert!(!is_key_repeat("\x1b[97;1:3u"));
        // Paste content that merely looks like a release event.
        assert!(!is_key_release("\x1b[200~90:62:3F:A5\x1b[201~"));
        assert!(!is_key_repeat("\x1b[200~x:2Fy\x1b[201~"));
    }

    #[test]
    fn decodes_printable_kitty_sequences() {
        assert_eq!(decode_printable_key("\x1b[97u"), Some('a'));
        assert_eq!(decode_printable_key("\x1b[65;2:65u"), Some('A'));
        // Ctrl is never a printable insertion.
        assert_eq!(decode_printable_key("\x1b[97;5u"), None);
        assert_eq!(decode_printable_key("\x1b[27;5;97~"), None);
        assert_eq!(decode_printable_key("\x1b[27;1;97~"), Some('a'));
        assert_eq!(decode_printable_key("\x1b[A"), None);
    }

    #[test]
    fn kitty_keypad_equivalents_match() {
        // KP_0 reports codepoint 57399; it must match the digit binding.
        assert!(matches_key("\x1b[57399;5u", "ctrl+0"));
        // KP_1 reports 57400 -> '1'.
        assert!(matches_key("\x1b[57400;5u", "ctrl+1"));
        // The keypad Enter is the same key as Enter.
        assert!(matches_key("\x1b[57414;1u", "enter"));
    }

    #[test]
    fn unknown_and_malformed_input_is_undefined() {
        assert_eq!(parse_key(""), None);
        assert_eq!(parse_key("\x1b[999999999999999999999u"), None);
        assert!(!matches_key("a", "not-a-key"));
        assert!(!matches_key("a", "ctrl+not-a-key"));
    }
}
