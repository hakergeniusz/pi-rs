//! Program Status Protocol (OSC 7501) — a verbatim port of upstream
//! `packages/tui/src/program-status.ts` (the whole file, 1-54), plus the
//! `replaceControlCharacters` helper it imports (`utils.ts:372-377`) and the
//! UTF-8 truncation limit it documents.
//!
//! A program tells the terminal whether it is idle, working, blocked on the
//! user, done, or failed. Only the root record is supported.
//!
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>
//!
//! Deviation: upstream builds the `msg` pair with node's
//! `Buffer.from(message, "utf8").toString("base64")`; the port reuses the
//! inline encoder from [`crate::terminal_image::base64_encode`] (same
//! alphabet, same padding), so no new dependency enters the crate. The reply
//! matcher and the `app` pattern are hand-rolled character scanners instead of
//! JavaScript regexes — the crate carries no regex engine — with the same
//! accept/reject sets as upstream's literals.

/// What a program reports. `ProgramStatus`, `program-status.ts:10-19`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProgramStatus {
    /// `clear` removes the status instead of reporting one.
    pub state: ProgramStatusState,
    /// Stable program name, `[A-Za-z0-9_.+-]{1,32}`. Other values are omitted.
    pub app: Option<String>,
    /// What a blocked program waits for. Omitted for other states.
    pub kind: Option<ProgramStatusKind>,
    /// One human-readable line. Control characters become spaces; longer text
    /// is cut to the spec limit.
    pub message: Option<String>,
}

/// `program-status.ts:12`. `"clear"` removes the status instead of reporting
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProgramStatusState {
    #[default]
    Idle,
    Working,
    Blocked,
    Done,
    Error,
    Clear,
}

impl ProgramStatusState {
    /// The wire spelling, identical to upstream's union members.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProgramStatusState::Idle => "idle",
            ProgramStatusState::Working => "working",
            ProgramStatusState::Blocked => "blocked",
            ProgramStatusState::Done => "done",
            ProgramStatusState::Error => "error",
            ProgramStatusState::Clear => "clear",
        }
    }
}

/// `program-status.ts:16`: what a blocked program waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusKind {
    Permission,
    Question,
    Auth,
}

impl ProgramStatusKind {
    /// The wire spelling, identical to upstream's union members.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProgramStatusKind::Permission => "permission",
            ProgramStatusKind::Question => "question",
            ProgramStatusKind::Auth => "auth",
        }
    }
}

/// Feature detection query. A supporting terminal replies with the same body.
/// `program-status.ts:22`.
pub const PROGRAM_STATUS_QUERY: &str = "\x1b]7501;?\x1b\\";

/// The reply to [`PROGRAM_STATUS_QUERY`]: `ESC ] 7501 ; ?` — any body without
/// BEL or ESC — closed by BEL or ST. Later spec revisions may add pairs after
/// the `?`. `program-status.ts:25-27`.
pub fn is_program_status_reply(sequence: &str) -> bool {
    let Some(rest) = sequence.strip_prefix("\x1b]7501;?") else {
        return false;
    };
    // `[^\x07\x1b]*(?:\x07|\x1b\\)$`: the body runs to the FIRST BEL or ESC,
    // and what follows from there must be exactly the terminator.
    match rest.find(['\x07', '\x1b']) {
        Some(index) => rest[index..] == *"\x07" || rest[index..] == *"\x1b\\",
        None => false,
    }
}

/// `^[A-Za-z0-9_.+-]{1,32}$` (`program-status.ts:29`).
fn app_pattern_matches(app: &str) -> bool {
    let bytes = app.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 {
        return false;
    }
    bytes.iter().all(|&b| {
        b.is_ascii_lowercase()
            || b.is_ascii_uppercase()
            || b.is_ascii_digit()
            || b == b'_'
            || b == b'.'
            || b == b'+'
            || b == b'-'
    })
}

/// `replaceControlCharacters` (`utils.ts:372-377`): each run of C0 and C1
/// control characters (ESC, BEL, newlines, ...) becomes one space.
pub fn replace_control_characters(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        let is_control = (c as u32) < 0x20 || ((c as u32) >= 0x7f && (c as u32) <= 0x9f);
        if is_control {
            in_run = true;
        } else {
            if in_run {
                out.push(' ');
                in_run = false;
            }
            out.push(c);
        }
    }
    if in_run {
        out.push(' ');
    }
    out
}

/// `truncateUtf8` (`program-status.ts:33-44`): cut on a character boundary so
/// the encoded form stays under `max_bytes`.
fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut bytes = 0usize;
    let mut end = 0usize;
    for c in text.chars() {
        let size = c.len_utf8();
        if bytes + size > max_bytes {
            break;
        }
        bytes += size;
        end += c.len_utf8();
    }
    text[..end].to_string()
}

/// Encode a status report. Terminals discard reports whose text contains
/// control characters, so they are replaced. `formatProgramStatus`,
/// `program-status.ts:47-54`.
pub fn format_program_status(status: &ProgramStatus) -> String {
    let mut pairs = vec![format!("state={}", status.state.as_str())];
    if let Some(app) = &status.app
        && app_pattern_matches(app)
    {
        pairs.push(format!("app={app}"));
    }
    if status.state == ProgramStatusState::Blocked
        && let Some(kind) = &status.kind
    {
        pairs.push(format!("kind={}", kind.as_str()));
    }
    let message = truncate_utf8(
        replace_control_characters(status.message.as_deref().unwrap_or("")).trim(),
        MAX_MESSAGE_BYTES,
    );
    if !message.is_empty() {
        pairs.push(format!(
            "msg={}",
            crate::terminal_image::base64_encode(message.as_bytes())
        ));
    }
    format!("\x1b]7501;{}\x1b\\", pairs.join(":"))
}

/// Decoded `msg` limit (`program-status.ts:31`). Its base64 encoding stays
/// under the 2732-byte encoded limit.
const MAX_MESSAGE_BYTES: usize = 2048;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_encodes_the_root_record() {
        let status = ProgramStatus {
            state: ProgramStatusState::Working,
            app: Some("pi".to_string()),
            kind: None,
            message: Some("Compacting context".to_string()),
        };
        // base64("Compacting context") = Q29tcGFjdGluZyBjb250ZXh0
        assert_eq!(
            format_program_status(&status),
            "\x1b]7501;state=working:app=pi:msg=Q29tcGFjdGluZyBjb250ZXh0\x1b\\"
        );
    }

    #[test]
    fn format_reports_kind_only_when_blocked() {
        let status = ProgramStatus {
            state: ProgramStatusState::Done,
            app: Some("pi".to_string()),
            kind: Some(ProgramStatusKind::Question),
            message: Some("Pick one".to_string()),
        };
        let encoded = format_program_status(&status);
        assert!(encoded.contains("state=done"), "{encoded}");
        assert!(!encoded.contains("kind="), "{encoded}");
        let blocked = ProgramStatus {
            state: ProgramStatusState::Blocked,
            app: Some("pi".to_string()),
            kind: Some(ProgramStatusKind::Permission),
            message: None,
        };
        assert_eq!(
            format_program_status(&blocked),
            "\x1b]7501;state=blocked:app=pi:kind=permission\x1b\\"
        );
    }

    #[test]
    fn format_drops_an_app_outside_the_pattern() {
        let status = ProgramStatus {
            state: ProgramStatusState::Idle,
            app: Some("bad app!".to_string()),
            kind: None,
            message: None,
        };
        assert_eq!(format_program_status(&status), "\x1b]7501;state=idle\x1b\\");
        let long = ProgramStatus {
            state: ProgramStatusState::Idle,
            app: Some("a".repeat(33)),
            kind: None,
            message: None,
        };
        assert_eq!(format_program_status(&long), "\x1b]7501;state=idle\x1b\\");
        let edge = ProgramStatus {
            state: ProgramStatusState::Idle,
            app: Some("a".repeat(32)),
            kind: None,
            message: None,
        };
        assert_eq!(
            format_program_status(&edge),
            format!("\x1b]7501;state=idle:app={}\x1b\\", "a".repeat(32))
        );
    }

    #[test]
    fn format_replaces_control_characters_in_the_message() {
        let status = ProgramStatus {
            state: ProgramStatusState::Error,
            app: None,
            kind: None,
            message: Some("line one\nline\ttwo\x1b]0;evil\x07".to_string()),
        };
        let encoded = format_program_status(&status);
        assert!(encoded.contains("state=error"), "{encoded}");
        // The cleaned text has no BEL/ESC left to break the sequence open.
        let body = encoded
            .trim_start_matches("\x1b]7501;")
            .trim_end_matches("\x1b\\");
        assert!(!body.contains('\x07'), "{encoded}");
        assert!(!body.contains('\x1b'), "{encoded}");
    }

    #[test]
    fn format_truncates_the_message_to_the_byte_limit() {
        // 2046 ASCII bytes plus three two-byte 'é's: the first 'é' fits
        // exactly (2048), the second would overflow, so the cut lands on a
        // character boundary after it — upstream's `truncateUtf8` rule.
        let message = format!("{}ééé", "x".repeat(2046));
        let status = ProgramStatus {
            state: ProgramStatusState::Working,
            app: None,
            kind: None,
            message: Some(message),
        };
        let encoded = format_program_status(&status);
        let msg_pair = encoded
            .split("msg=")
            .nth(1)
            .expect("msg pair present")
            .trim_end_matches("\x1b\\");
        let decoded = test_base64_decode(msg_pair);
        assert_eq!(decoded.len(), 2048, "cut at the byte limit");
        assert!(std::str::from_utf8(&decoded).is_ok(), "character boundary");
    }

    /// A minimal base64 decoder for test assertions (the crate has no decode
    /// facility; `terminal_image` only encodes).
    fn test_base64_decode(encoded: &str) -> Vec<u8> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for c in encoded.bytes() {
            // The encoder pads with `=`; padding carries no bits.
            if c == b'=' {
                break;
            }
            let value = ALPHABET
                .iter()
                .position(|&a| a == c)
                .expect("test input is canonical base64") as u32;
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push(((buffer >> bits) & 0xff) as u8);
            }
        }
        out
    }

    #[test]
    fn reply_matcher_accepts_the_query_reply_shape() {
        assert!(is_program_status_reply("\x1b]7501;?\x1b\\"));
        assert!(is_program_status_reply("\x1b]7501;?state=idle\x07"));
        assert!(is_program_status_reply(
            "\x1b]7501;?state=idle:app=pi\x1b\\"
        ));
    }

    #[test]
    fn reply_matcher_rejects_unrelated_sequences() {
        assert!(!is_program_status_reply("\x1b]10;rgb:ffff/ffff/ffff\x1b\\"));
        assert!(!is_program_status_reply("\x1b]7501;state=idle\x1b\\"));
        assert!(!is_program_status_reply("\x1b]7501;?unterminated"));
        assert!(!is_program_status_reply("\x1b[?1;2c"));
        assert!(!is_program_status_reply(""));
    }

    #[test]
    fn control_runs_collapse_to_one_space() {
        assert_eq!(replace_control_characters("a\nb"), "a b");
        assert_eq!(replace_control_characters("a\r\n\tb"), "a b");
        assert_eq!(replace_control_characters("a\x1b]0;x\x07b"), "a ]0;x b");
        assert_eq!(replace_control_characters("ab"), "ab");
        assert_eq!(replace_control_characters("\x7f\u{9f}"), " ");
    }
}
