//! stdin reassembly: split chunked input into complete sequences, capture
//! bracketed pastes.
//!
//! Ground truth: `packages/tui/src/stdin-buffer.ts` (444 lines).
//!
//! The port is clock-injectable: there is no timer thread. The host asks
//! [`StdinBuffer::pending_timeout_ms`] how long it may wait and calls
//! [`StdinBuffer::flush`] when that time passed, which is exactly what pi's
//! `setTimeout` does, minus the event loop.

/// An event emitted by the buffer. `stdin-buffer.ts:257-261`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinEvent {
    /// One complete key sequence or one printable character.
    Data(String),
    /// The contents of a bracketed paste, without the markers.
    Paste(String),
}

/// Default wait for an incomplete CSI/mouse sequence. `stdin-buffer.ts:23`.
pub const DEFAULT_SEQUENCE_TIMEOUT_MS: u64 = 50;
/// Default wait before treating a lone ESC as the Escape key.
/// `stdin-buffer.ts:24`.
pub const DEFAULT_ESCAPE_TIMEOUT_MS: u64 = 10;
/// Wait for Alt+key input over high-latency transports (SSH). `terminal.ts:114-129`.
pub const DEFAULT_SSH_ESCAPE_TIMEOUT_MS: u64 = 100;

const BRACKETED_PASTE_START: &str = "\x1b[200~";
const BRACKETED_PASTE_END: &str = "\x1b[201~";

/// Completeness of a candidate escape sequence. `stdin-buffer.ts:31-80`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceState {
    Complete,
    Incomplete,
    NotEscape,
}

/// `stdin-buffer.ts:86-128`. CSI runs to a final byte in `0x40..=0x7E`; SGR
/// mouse reports (`ESC [ < b ; x ; y M/m`) need their exact shape.
fn is_complete_csi(data: &str) -> SequenceState {
    if !data.starts_with("\x1b[") {
        return SequenceState::Complete;
    }
    if data.len() < 3 {
        return SequenceState::Incomplete;
    }
    let payload = &data[2..];
    let Some(last) = payload.chars().last() else {
        return SequenceState::Incomplete;
    };
    if !('\u{40}'..='~').contains(&last) {
        return SequenceState::Incomplete;
    }
    if payload.starts_with('<') {
        let is_mouse = payload.strip_prefix('<').is_some_and(|rest| {
            let body = &rest[..rest.len() - 1];
            let parts: Vec<&str> = body.split(';').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        });
        if is_mouse {
            return SequenceState::Complete;
        }
        // Ends with M/m but is not a well-formed mouse report yet.
        return SequenceState::Incomplete;
    }
    SequenceState::Complete
}

/// `stdin-buffer.ts:31-80`.
fn sequence_state(data: &str) -> SequenceState {
    if !data.starts_with('\x1b') {
        return SequenceState::NotEscape;
    }
    if data.len() == 1 {
        return SequenceState::Incomplete;
    }
    let after_esc = &data[1..];
    if after_esc.starts_with('[') {
        // Old-style mouse report: ESC [ M + 3 bytes.
        if after_esc.starts_with("[M") {
            return if data.len() >= 6 {
                SequenceState::Complete
            } else {
                SequenceState::Incomplete
            };
        }
        return is_complete_csi(data);
    }
    // OSC / DCS / APC run to BEL or ST; SS3 is ESC O + one character.
    if after_esc.starts_with(']') {
        return osc_state(data);
    }
    if after_esc.starts_with('P') {
        return st_state(data);
    }
    if after_esc.starts_with('_') {
        return st_state(data);
    }
    if after_esc.starts_with('O') {
        return if after_esc.chars().count() >= 2 {
            SequenceState::Complete
        } else {
            SequenceState::Incomplete
        };
    }
    // Meta key: ESC plus a single character.
    if after_esc.chars().count() == 1 {
        return SequenceState::Complete;
    }
    SequenceState::Complete
}

/// `stdin-buffer.ts:134-145`.
fn osc_state(data: &str) -> SequenceState {
    if data.ends_with('\x07') || data.ends_with("\x1b\\") {
        SequenceState::Complete
    } else {
        SequenceState::Incomplete
    }
}

/// `stdin-buffer.ts:152-181`. DCS and APC share the ST terminator.
fn st_state(data: &str) -> SequenceState {
    if data.ends_with("\x1b\\") {
        SequenceState::Complete
    } else {
        SequenceState::Incomplete
    }
}

/// Unmodified Kitty printable codepoint carried by a sequence, if any.
/// `stdin-buffer.ts:186-192`.
fn parse_unmodified_kitty_printable_codepoint(sequence: &str) -> Option<u32> {
    let body = sequence.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let codepoint_text = body.split(':').next()?;
    if codepoint_text.is_empty() || !codepoint_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Only the unmodified form (`\x1b[<codepoint>u`) swallows the next byte.
    if body.contains(':') || body.contains(';') {
        return None;
    }
    let codepoint = codepoint_text.parse::<u32>().ok()?;
    (codepoint >= 32).then_some(codepoint)
}

/// Split the buffer into complete sequences, leaving any partial tail.
/// `stdin-buffer.ts:194-252`.
fn extract_complete_sequences(buffer: &str) -> (Vec<String>, String) {
    let mut sequences: Vec<String> = Vec::new();
    let mut pos = 0usize;
    while pos < buffer.len() {
        let remaining = &buffer[pos..];
        if remaining.starts_with('\x1b') {
            let mut seq_end = 1usize;
            let mut emitted = false;
            while seq_end <= remaining.len() {
                let candidate = &remaining[..seq_end];
                match sequence_state(candidate) {
                    SequenceState::Complete => {
                        // WezTerm sends Escape as a bare ESC and the release
                        // as CSI-u, arriving as `\x1b\x1b[27;…u`. Treat
                        // `\x1b\x1b` as a lone Escape when the next byte would
                        // start another escape sequence.
                        if candidate == "\x1b\x1b" {
                            if let Some(next) = remaining[seq_end..].chars().next() {
                                if "[ ]OP_".contains(next) {
                                    sequences.push("\x1b".to_string());
                                    pos += 1;
                                    emitted = true;
                                    break;
                                }
                            }
                        }
                        sequences.push(candidate.to_string());
                        pos += seq_end;
                        emitted = true;
                        break;
                    }
                    SequenceState::Incomplete => seq_end += 1,
                    SequenceState::NotEscape => {
                        sequences.push(candidate.to_string());
                        pos += seq_end;
                        emitted = true;
                        break;
                    }
                }
            }
            if seq_end > remaining.len() {
                return (sequences, remaining.to_string());
            }
            if emitted {
                continue;
            }
            // Nothing consumed and no progress possible: bail out with the tail.
            return (sequences, remaining.to_string());
        }
        let ch = remaining.chars().next().expect("non-empty");
        sequences.push(ch.to_string());
        pos += ch.len_utf8();
    }
    (sequences, String::new())
}

/// Buffer options. `stdin-buffer.ts:255-269`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdinBufferOptions {
    /// Maximum wait for an incomplete sequence. Default 50 ms.
    pub timeout_ms: u64,
    /// Maximum wait after a lone ESC. Default 10 ms; SSH uses 100 ms.
    pub escape_timeout_ms: u64,
}

impl Default for StdinBufferOptions {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_SEQUENCE_TIMEOUT_MS,
            escape_timeout_ms: DEFAULT_ESCAPE_TIMEOUT_MS,
        }
    }
}

/// `stdin-buffer.ts:272-...`.
#[derive(Debug, Clone)]
pub struct StdinBuffer {
    buffer: String,
    options: StdinBufferOptions,
    paste_mode: bool,
    paste_buffer: String,
    pending_kitty_printable_codepoint: Option<u32>,
}

impl StdinBuffer {
    /// `stdin-buffer.ts:283-289`.
    pub fn new(options: StdinBufferOptions) -> Self {
        Self {
            buffer: String::new(),
            options,
            paste_mode: false,
            paste_buffer: String::new(),
            pending_kitty_printable_codepoint: None,
        }
    }

    /// Escape timeout that fits the transport. `terminal.ts:113-129`.
    pub fn ssh_aware(over_escape_timeout_ms: Option<u64>) -> Self {
        let escape_timeout_ms = match over_escape_timeout_ms {
            Some(value) if value > 0 => value,
            _ => {
                // pi's truthiness is `Boolean(value)` (`keys.ts:717`), so an empty
                // variable counts as unset. `keys.rs:480` is the same rule.
                let set =
                    |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
                if set("SSH_CONNECTION") || set("SSH_TTY") {
                    DEFAULT_SSH_ESCAPE_TIMEOUT_MS
                } else {
                    DEFAULT_ESCAPE_TIMEOUT_MS
                }
            }
        };
        Self::new(StdinBufferOptions {
            escape_timeout_ms,
            ..Default::default()
        })
    }

    /// Feed a chunk of stdin and append the events it completes.
    /// `stdin-buffer.ts:297-...`.
    pub fn process(&mut self, data: &str, out: &mut Vec<StdinEvent>) {
        if data.is_empty() && self.buffer.is_empty() {
            out.push(StdinEvent::Data(String::new()));
            return;
        }
        self.buffer.push_str(data);

        if self.paste_mode {
            self.paste_buffer.push_str(&self.buffer);
            self.buffer.clear();
            self.drain_paste(out);
            return;
        }

        if let Some(start) = self.buffer.find(BRACKETED_PASTE_START) {
            if start > 0 {
                let (sequences, _) = extract_complete_sequences(&self.buffer[..start]);
                for sequence in sequences {
                    self.emit_data_sequence(&sequence, out);
                }
            }
            self.pending_kitty_printable_codepoint = None;
            self.buffer = self.buffer[start + BRACKETED_PASTE_START.len()..].to_string();
            self.paste_mode = true;
            self.paste_buffer = std::mem::take(&mut self.buffer);
            self.drain_paste(out);
            return;
        }

        let (sequences, remainder) = extract_complete_sequences(&self.buffer);
        self.buffer = remainder;
        for sequence in sequences {
            self.emit_data_sequence(&sequence, out);
        }
    }

    /// Emit a whole paste when its terminator arrived. `stdin-buffer.ts:317-330`
    /// and `:344-357`.
    fn drain_paste(&mut self, out: &mut Vec<StdinEvent>) {
        let Some(end) = self.paste_buffer.find(BRACKETED_PASTE_END) else {
            return;
        };
        let content = self.paste_buffer[..end].to_string();
        let remaining = self.paste_buffer[end + BRACKETED_PASTE_END.len()..].to_string();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable_codepoint = None;
        out.push(StdinEvent::Paste(content));
        if !remaining.is_empty() {
            self.process(&remaining, out);
        }
    }

    /// Drop a Kitty CSI-u printable that the terminal also sent as a raw byte.
    /// `stdin-buffer.ts:369-378`.
    fn emit_data_sequence(&mut self, sequence: &str, out: &mut Vec<StdinEvent>) {
        let raw_codepoint = if sequence.chars().count() == 1 {
            sequence.chars().next().map(|ch| ch as u32)
        } else {
            None
        };
        if raw_codepoint.is_some() && raw_codepoint == self.pending_kitty_printable_codepoint {
            self.pending_kitty_printable_codepoint = None;
            return;
        }
        self.pending_kitty_printable_codepoint =
            parse_unmodified_kitty_printable_codepoint(sequence);
        out.push(StdinEvent::Data(sequence.to_string()));
    }

    /// How long the host may wait before calling [`StdinBuffer::flush`].
    /// `stdin-buffer.ts:380-390`.
    pub fn pending_timeout_ms(&self) -> Option<u64> {
        if self.buffer.is_empty() {
            return None;
        }
        Some(if self.buffer == "\x1b" {
            self.options.escape_timeout_ms
        } else {
            self.options.timeout_ms
        })
    }

    /// Emit whatever is buffered as one sequence. `stdin-buffer.ts:392-405`.
    pub fn flush(&mut self, out: &mut Vec<StdinEvent>) {
        if self.buffer.is_empty() {
            return;
        }
        let buffer = std::mem::take(&mut self.buffer);
        self.pending_kitty_printable_codepoint = None;
        out.push(StdinEvent::Data(buffer));
    }

    /// `stdin-buffer.ts:407-416`.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.paste_mode = false;
        self.paste_buffer.clear();
        self.pending_kitty_printable_codepoint = None;
    }

    /// `stdin-buffer.ts:418-420`.
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Whether a paste is being accumulated.
    pub fn in_paste_mode(&self) -> bool {
        self.paste_mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(buffer: &mut StdinBuffer, chunks: &[&str]) -> Vec<StdinEvent> {
        let mut events = Vec::new();
        for chunk in chunks {
            buffer.process(chunk, &mut events);
        }
        events
    }

    #[test]
    fn splits_sequences_across_chunks() {
        let mut buffer = StdinBuffer::default_in();
        let events = feed(&mut buffer, &["\x1b", "[3", "5~x"]);
        assert_eq!(
            events,
            vec![
                StdinEvent::Data("\x1b[35~".to_string()),
                StdinEvent::Data("x".to_string())
            ]
        );
    }

    #[test]
    fn lone_escape_waits_for_the_escape_timeout() {
        let mut buffer = StdinBuffer::default_in();
        let events = feed(&mut buffer, &["\x1b"]);
        assert!(events.is_empty());
        assert_eq!(buffer.pending_timeout_ms(), Some(DEFAULT_ESCAPE_TIMEOUT_MS));

        let mut events = Vec::new();
        buffer.flush(&mut events);
        assert_eq!(events, vec![StdinEvent::Data("\x1b".to_string())]);
        assert_eq!(buffer.pending_timeout_ms(), None);
    }

    #[test]
    fn partial_sequence_uses_the_sequence_timeout() {
        let mut buffer = StdinBuffer::default_in();
        feed(&mut buffer, &["\x1b[<35;20"]);
        assert_eq!(
            buffer.pending_timeout_ms(),
            Some(DEFAULT_SEQUENCE_TIMEOUT_MS)
        );
    }

    #[test]
    fn captures_bracketed_paste_whole() {
        let mut buffer = StdinBuffer::default_in();
        let events = feed(&mut buffer, &["\x1b[200~hello\nworld\x1b[201~"]);
        assert_eq!(events, vec![StdinEvent::Paste("hello\nworld".to_string())]);
        assert!(!buffer.in_paste_mode());
    }

    #[test]
    fn paste_split_over_chunks_is_captured() {
        let mut buffer = StdinBuffer::default_in();
        let events = feed(&mut buffer, &["\x1b[200~he", "llo\x1b[20", "1~"]);
        assert_eq!(events, vec![StdinEvent::Paste("hello".to_string())]);
    }

    #[test]
    fn keys_before_a_paste_are_emitted_first() {
        let mut buffer = StdinBuffer::default_in();
        let events = feed(&mut buffer, &["a\x1b[200~pasted\x1b[201~"]);
        assert_eq!(
            events,
            vec![
                StdinEvent::Data("a".to_string()),
                StdinEvent::Paste("pasted".to_string())
            ]
        );
    }

    #[test]
    fn input_after_the_paste_terminator_is_processed() {
        let mut buffer = StdinBuffer::default_in();
        let events = feed(&mut buffer, &["\x1b[200~x\x1b[201~y"]);
        assert_eq!(
            events,
            vec![
                StdinEvent::Paste("x".to_string()),
                StdinEvent::Data("y".to_string())
            ]
        );
    }

    #[test]
    fn wezterm_double_escape_splits_into_escape_then_release() {
        let mut buffer = StdinBuffer::default_in();
        let events = feed(&mut buffer, &["\x1b\x1b[27;5;97:3u"]);
        assert_eq!(
            events,
            vec![
                StdinEvent::Data("\x1b".to_string()),
                StdinEvent::Data("\x1b[27;5;97:3u".to_string()),
            ]
        );
    }

    #[test]
    fn unmodified_kitty_printable_is_not_duplicated() {
        let mut buffer = StdinBuffer::default_in();
        // The terminal sends CSI-u for `a` and then the raw byte; the raw
        // byte must not be forwarded twice.
        let events = feed(&mut buffer, &["\x1b[97u", "a"]);
        assert_eq!(events, vec![StdinEvent::Data("\x1b[97u".to_string())]);
    }

    #[test]
    fn osc_and_dcs_sequences_wait_for_their_terminator() {
        let mut buffer = StdinBuffer::default_in();
        let events = feed(&mut buffer, &["\x1b]10;rgb:ff/00", "/00\x07"]);
        assert_eq!(
            events,
            vec![StdinEvent::Data("\x1b]10;rgb:ff/00/00\x07".to_string())]
        );
        let mut buffer = StdinBuffer::default_in();
        let events = feed(&mut buffer, &["\x1bP>|pi(1.0)\x1b\\"]);
        assert_eq!(
            events,
            vec![StdinEvent::Data("\x1bP>|pi(1.0)\x1b\\".to_string())]
        );
    }

    #[test]
    fn empty_input_without_buffer_emits_an_empty_sequence() {
        let mut buffer = StdinBuffer::default_in();
        assert_eq!(
            feed(&mut buffer, &[""]),
            vec![StdinEvent::Data(String::new())]
        );
    }

    #[test]
    fn clear_resets_paste_state() {
        let mut buffer = StdinBuffer::default_in();
        feed(&mut buffer, &["\x1b[200~partial"]);
        assert!(buffer.in_paste_mode());
        buffer.clear();
        assert!(!buffer.in_paste_mode());
        assert_eq!(buffer.buffer(), "");
    }

    impl StdinBuffer {
        fn default_in() -> Self {
            Self::new(StdinBufferOptions::default())
        }
    }
}
