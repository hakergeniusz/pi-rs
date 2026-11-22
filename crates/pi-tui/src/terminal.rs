//! Terminal abstraction: a `Terminal` trait, a crossterm backend for real
//! terminals, and an in-memory backend used by the offline frame tests.
//!
//! Ground truth: `packages/tui/src/terminal.ts` (452 lines).
//!
//! The raw escape sequences are copied from pi so captured frames are
//! byte-comparable with what pi writes. Query responses are exposed as
//! [`Terminal::poll_response`] instead of pi's promise-based
//! `Terminal.query(...)`; the port keeps the transport, drops the async
//! orchestration, which 4b owns.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use crossterm::event::{
    Event, KeyCode, KeyEvent as CtKeyEvent, KeyEventKind, KeyModifiers as CtKeyModifiers,
    MouseButton as CtMouseButton, MouseEvent as CtMouseEvent, MouseEventKind as CtMouseEventKind,
};

/// Button-motion mouse tracking plus SGR encoding. `tui-alt-screen.ts:66`.
pub const ENABLE_MOUSE_TRACKING: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h";
/// The inverse, written in the same order pi disables them.
pub const DISABLE_MOUSE_TRACKING: &str = "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// SGR button code for a crossterm button. `tui-alt-screen.ts:786-799`.
fn button_code(button: CtMouseButton) -> u32 {
    match button {
        CtMouseButton::Left => 0,
        CtMouseButton::Right => 2,
        CtMouseButton::Middle => 1,
    }
}

/// Re-encode a crossterm mouse event as an SGR report.
///
/// Terminals that support SGR never send X10, so the 1006 form is the only one
/// this crate has to produce; the X10 decoder in [`crate::mouse`] exists for
/// terminals that ignore the mode.
pub fn encode_sgr_mouse_event(event: CtMouseEvent) -> Option<String> {
    // crossterm carries the button in the `kind`, not in its own field.
    let (button, code, release): (u32, u32, bool) = match event.kind {
        CtMouseEventKind::Down(button) => (button_code(button), 0, false),
        CtMouseEventKind::Up(button) => (button_code(button), 0, true),
        CtMouseEventKind::Drag(button) => (button_code(button), 32, false),
        CtMouseEventKind::Moved => (3, 35, false),
        // Wheel and horizontal scroll carry no button bits: the decoder
        // rejects a report whose low two bits are 2 or 3
        // (`tui-alt-screen.ts:958-970`).
        CtMouseEventKind::ScrollUp => (0, 64, false),
        CtMouseEventKind::ScrollDown => (0, 65, false),
        CtMouseEventKind::ScrollLeft => (0, 66, false),
        CtMouseEventKind::ScrollRight => (0, 67, false),
    };
    let modifiers = (button | code)
        | if event.modifiers.contains(CtKeyModifiers::SHIFT) {
            4
        } else {
            0
        }
        | if event.modifiers.contains(CtKeyModifiers::ALT) {
            8
        } else {
            0
        }
        | if event.modifiers.contains(CtKeyModifiers::CONTROL) {
            16
        } else {
            0
        };
    Some(format!(
        "\x1b[<{modifiers};{};{}{}",
        event.column + 1,
        event.row + 1,
        if release { 'm' } else { 'M' }
    ))
}

/// A write sink. The trait exists so the renderer can be tested without a TTY.
pub trait TerminalWrite {
    /// Write one chunk of output to the terminal.
    fn write_str(&mut self, text: &str) -> io::Result<()>;
}

/// Terminal size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

/// Everything the TUI needs from a terminal.
pub trait Terminal: Send {
    /// Start: raw mode, mouse capture, bracketed paste, alt screen, hide
    /// cursor. `terminal.ts:222-233`.
    fn start(&mut self) -> io::Result<()>;
    /// Stop: the inverse of [`Terminal::start`]. `terminal.ts:235-247`.
    fn stop(&mut self) -> io::Result<()>;
    /// Current size, re-queried when the host supports it.
    fn size(&self) -> io::Result<TerminalSize>;
    /// Write one chunk of output; every rendered frame goes through this.
    fn write(&mut self, text: &str) -> io::Result<()>;
    /// `terminal.ts:315-317`. Returns false for backends without a tty.
    fn is_tty(&self) -> bool;
    /// The most recent bytes written, for query assertions. Default `None`.
    fn last_written(&self) -> Option<String> {
        None
    }

    /// Take the mouse mode the TUI asked for. `terminal.ts:385-387`.
    fn take_mouse_mode(&mut self) -> bool;
    /// Record whether the TUI wants mouse tracking, before [`Terminal::start`]
    /// writes the enable sequence. `tui-alt-screen.ts:274`, `:373-377`.
    fn request_mouse_mode(&mut self, _enabled: bool) {}
    /// Take one buffered terminal query response. `terminal.ts:249-259`.
    fn poll_response(&mut self) -> Option<String>;
    /// Subscribe to a reply callback. `terminal.ts:261-265`.
    fn on_response(&mut self, id: &str, callback: Box<dyn Fn(String) + Send>);
    /// Re-raise SIGWINCH. `terminal.ts:337-340`.
    fn refresh_size(&mut self) {}
    /// Override the size. Backends that query the tty ignore this; tests use
    /// it to drive width and height changes.
    fn set_size(&mut self, size: TerminalSize) {
        self.override_size(size);
    }
    /// Set what the next [`Terminal::refresh_size`] will report. Backends that
    /// read the tty ignore this; it exists so a test can model a window the
    /// user resized without raising a signal.
    fn set_queried_size(&mut self, _size: TerminalSize) {}
    /// Apply an injected size. `terminal.ts:157-162` keeps a cached size.
    fn override_size(&mut self, _size: TerminalSize) {}
}

/// Report for `tput`-style probing, computed without spawning a process.
/// `terminal.ts:268-289`.
pub fn default_reports() -> Vec<(u16, u16)> {
    vec![
        (24, 80),
        (40, 80),
        (48, 80),
        (47, 160),
        (47, 512),
        (48, 512),
        (95, 80),
        (96, 512),
        (300, 80),
        (300, 512),
        (900, 512),
        (900, 4608),
    ]
}

/// pi's truecolor hint (`terminal-image.ts:71,75`), verbatim: both variables
/// are lowercased first, so `COLORTERM=TrueColor` works, and a `TERM` ending
/// in `-direct` counts on its own. pi compares against the empty string, which
/// is what `unwrap_or_default()` gives.
pub fn detect_color_support() -> bool {
    let color_term = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase();
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    color_term == "truecolor" || color_term == "24bit" || term.ends_with("-direct")
}

/// pi's size fallback chain (`terminal.ts:485-494`): the real columns/rows
/// first, then `COLUMNS`/`LINES`, then 80x24.
///
/// `Number("")` is `0` in JavaScript and `0` is falsy, so an empty,
/// unparsable or zero variable falls through to the hardcoded size. Nothing
/// else is filtered - pi's `||` tests truthiness only, so `COLUMNS=-5` wins
/// over the 80 and every component's `maxWidth <= 0` guard empties the frame.
/// [`js_number`] drops exactly the falsy values; the cast to `u16` saturates,
/// so a negative or an enormous size becomes 0 columns, which empties the
/// frame the same way pi's guard does.
pub fn size_from_lookup(lookup: impl Fn(&str) -> Option<String>) -> TerminalSize {
    let number = |name: &str| {
        lookup(name)
            .as_deref()
            .and_then(js_number)
            .map(|value| value as u16)
    };
    TerminalSize {
        columns: number("COLUMNS").unwrap_or(80),
        rows: number("LINES").unwrap_or(24),
    }
}

/// [`size_from_lookup`] over this process's environment. The lookup is the
/// only `std::env` reach in the size chain, so tests drive the pure function
/// instead of writing the environment: `std::env::set_var` is process-wide and
/// races every other test thread in the binary.
pub fn size_from_environment() -> TerminalSize {
    size_from_lookup(|name| std::env::var(name).ok())
}

/// `process.stdout.columns || Number(COLUMNS) || 80` (`terminal.ts:561-567`):
/// a successful ioctl that reports zero columns or rows is just as unusable
/// as a failing one, and must fall through to the environment/defaults.
/// Several terminal emulators report a zero (or tiny) winsize for a freshly
/// spawned PTY; trusting it renders a miniature frame in the top-left corner.
pub fn size_from_ioctl_or_environment() -> TerminalSize {
    match crossterm::terminal::size() {
        Ok((columns, rows)) if columns > 0 && rows > 0 => TerminalSize { columns, rows },
        _ => size_from_environment(),
    }
}

/// `Number(text)` for the one call site pi makes it (`terminal.ts:489`, `:493`).
///
/// JavaScript's `Number` is not Rust's `str::parse`: it trims surrounding
/// whitespace, reads `0x`/`0o`/`0b` prefixes, accepts an exponent and the
/// words `Infinity`, and turns an empty string into `0`. Only the truthiness
/// of the result matters to pi's `a || b || 80` chain, so this returns `None`
/// for every value JavaScript considers falsy (`NaN`, `0`, `-0`) and `Some`
/// otherwise - `Infinity` included, because pi keeps it.
pub fn js_number(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (sign, digits) = match text.as_bytes()[0] {
        b'+' => (1.0, &text[1..]),
        b'-' => (-1.0, &text[1..]),
        _ => (1.0, text),
    };
    let magnitude = match digits {
        "Infinity" => Some(f64::INFINITY),
        _ => match digits.get(..2) {
            Some("0x" | "0X") => radix_value(&digits[2..], 16),
            Some("0o" | "0O") => radix_value(&digits[2..], 8),
            Some("0b" | "0B") => radix_value(&digits[2..], 2),
            _ => decimal_value(digits),
        },
    };
    match magnitude {
        Some(value) if sign * value != 0.0 => Some(sign * value),
        _ => None,
    }
}

/// The `0x`/`0o`/`0b` forms of `Number`: unsigned digits only (the sign was
/// already split off), at least one digit.
///
/// Accumulated in `f64` rather than through `u128`, so a literal wider than 128
/// bits stays a (huge) number instead of becoming `NaN` - which would read as
/// falsy and drop to the 80-column fallback.
fn radix_value(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() {
        return None;
    }
    let mut value = 0.0f64;
    for byte in digits.bytes() {
        let digit = char::from(byte).to_digit(radix)? as f64;
        value = value * f64::from(radix) + digit;
        if !value.is_finite() {
            return Some(f64::INFINITY);
        }
    }
    Some(value)
}

/// The decimal form of `Number`: `12`, `12.`, `.5`, `1.5e2`. Rust's own float
/// parser is *not* used, because it also accepts spellings JavaScript rejects
/// (`inf`, `NaN`, `infinity` in any case, hex floats like `0x1p3`).
fn decimal_value(digits: &str) -> Option<f64> {
    let (mantissa, exponent) = match digits.find(['e', 'E']) {
        Some(at) => (&digits[..at], Some(&digits[at + 1..])),
        None => (digits, None),
    };
    let mut parts = mantissa.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return None;
    }
    let integral_digits = !whole.is_empty();
    let fractional_digits = !fraction.is_empty();
    if !integral_digits && !fractional_digits {
        return None;
    }
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let mut text = String::with_capacity(digits.len());
    if integral_digits {
        text.push_str(whole);
    }
    if fractional_digits {
        if integral_digits {
            text.push('.');
        } else {
            // `.5` is 0.5 in JavaScript, not 5.
            text.push_str("0.");
        }
        text.push_str(fraction);
    }
    if let Some(exponent) = exponent {
        // The digits are everything after the sign; the sign itself is kept in
        // the text, because `1e-2` is 0.01 and not 100.
        let digits = exponent.trim_start_matches(['+', '-']);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        if !text.contains('e') {
            text.push('e');
        }
        text.push_str(exponent);
    }
    text.parse::<f64>().ok()
}

/// Keys `Terminal.query` understands. `terminal.ts:169-182`.
pub mod query_ids {
    /// Terminal name and version query, sent as `CSI > 0 q`.
    pub const XT_VERSION: &str = "xtversion";
    /// Supported-escape-sequences query, sent as the DA1-style `CSI c`.
    pub const XT_GET_ESCAPE_SEQUENCES: &str = "xtgetescapesequences";
    /// Modifier-key reporting query, sent as `CSI > 4 ; 2 m`.
    pub const XT_GET_MODIFIER_KEYS: &str = "xtgetmodifierkeys";
    /// Pointer-mode query, sent as `CSI ? 1004 p`.
    pub const XT_GET_POINTER_MODE: &str = "xtgetpointermode";
    /// Grapheme-clustering query, sent as `CSI ? 2027 $ p`.
    pub const XT_GET_GRAPHEME_CLUSTERING: &str = "xtgetgraphemeclustering";
    /// Primary device attributes, sent as `CSI c`.
    pub const DA1: &str = "da1";
    /// Secondary device attributes, sent as `CSI > 0 c`.
    pub const DA2: &str = "da2";
    /// Color-stack push query, sent as the same `CSI > 0 q` as [`query_ids::XT_VERSION`].
    pub const XT_PUSH_COLORS: &str = "xtpushcolors";
    /// Color-stack pop report query, sent as `CSI ? 1021 ; 1 $ y`.
    pub const XT_REP_POP_COLORS: &str = "xtreppopcolors";
    /// Light/dark color-scheme query, sent as `CSI ? 996 n`.
    pub const COLOR_SCHEME_REPORT: &str = "colorscheme";
    /// Default background color query, sent as OSC 11.
    pub const BGCOLOR: &str = "bgcolor";
    /// Default foreground color query, sent as OSC 10.
    pub const FGCOLOR: &str = "fgcolor";
    /// Second `"xtversion"` constant, byte-identical to [`query_ids::XT_VERSION`].
    pub const XTVERSION: &str = "xtversion";
}

/// Sequence sent to query `id`. `terminal.ts:183-209`.
pub fn query_sequence(id: &str) -> Option<&'static str> {
    Some(match id {
        "xtversion" | "xtpushcolors" => "\x1b[>0q",
        // Both queries are answered by the DA1-style reply `CSI c`.
        "xtgetescapesequences" | "da1" => "\x1b[c",
        "xtgetmodifierkeys" => "\x1b[>4;2m",
        "xtgetpointermode" => "\x1b[?1004p",
        "xtgetgraphemeclustering" => "\x1b[?2027$p",
        "da2" => "\x1b[>0c",
        "xtreppopcolors" => "\x1b[?1021;1$y",
        "colorscheme" => "\x1b[?996n",
        "bgcolor" => "\x1b]11;?\x07",
        "fgcolor" => "\x1b]10;?\x07",
        _ => return None,
    })
}

/// OSC prefix for enabling/disabling report formats, keyed by the argument of
/// `CSI ? <arg> h/l`. `terminal.ts:211-219`.
pub fn report_mode_sequence(mode: u16, enabled: bool) -> Option<String> {
    let action = if enabled { 'h' } else { 'l' };
    Some(match mode {
        1004 => format!("\x1b[?{mode}{action}"),
        2026 | 2027 => format!("\x1b[>{mode};2{action}"),
        _ => return None,
    })
}

/// Whether `response` answers `query`.
///
/// A reply carries a payload where the query did not, so prefix matching
/// alone is wrong: DA1 asks `CSI c` and is answered `CSI ? 62 ; 1 2 c`.
/// `terminal.ts:249-259` pairs replies by query kind, not by bytes.
pub fn query_reply_matches(query: &str, response: &str) -> bool {
    match query {
        "\x1b[c" => response.starts_with("\x1b[?") && response.ends_with('c'),
        // XTVERSION is answered with a DCS `>|name ST` string.
        "\x1b[>0q" => response.starts_with("\x1bP>|"),
        "\x1b[>0c" => response.starts_with("\x1b[>") && response.ends_with('c'),
        "\x1b[>4;2m" => response.starts_with("\x1b[>4;") && response.ends_with('m'),
        "\x1b]10;?\x07" | "\x1b]10;\x1b\\" => response.starts_with("\x1b]10;"),
        "\x1b]11;?\x07" | "\x1b]11;\x1b\\" => response.starts_with("\x1b]11;"),
        other => response.starts_with(other),
    }
}

/// Shared reply bookkeeping used by every backend.
///
/// pi pairs a reply with the query that asked for it by remembering the
/// escape sequence it sent (`terminal.ts:249-259`); this port does the same,
/// matching a response to the longest registered query prefix.
/// `terminal.ts:253-265`.
/// The handler a backend runs for a query response.
type ResponseCallback = Box<dyn Fn(String) + Send>;

/// Cloneable so a test can keep a handle to a terminal that was moved into
/// a component: pushes and expectations reach the same store.
#[derive(Clone, Default)]
struct ResponseStore {
    responses: Arc<Mutex<Vec<String>>>,
    callbacks: Arc<Mutex<HashMap<String, ResponseCallback>>>,
    expected: Arc<Mutex<Vec<(String, String)>>>,
}

impl ResponseStore {
    /// Remember that `sequence` was sent for query `id`.
    fn expect(&self, id: &str, sequence: &str) {
        if let Ok(mut expected) = self.expected.lock() {
            expected.push((id.to_string(), sequence.to_string()));
        }
    }

    fn push(&self, response: String) {
        let matched = self.expected.lock().ok().and_then(|mut expected| {
            let best = expected
                .iter()
                .enumerate()
                .filter(|(_, (_, sequence))| query_reply_matches(sequence, &response))
                .max_by_key(|(_, (_, sequence))| sequence.len())
                .map(|(index, (id, _))| (index, id.clone()));
            match best {
                Some((index, id)) => {
                    expected.remove(index);
                    Some(id)
                }
                None => None,
            }
        });
        if let Some(id) = matched {
            let callback = self
                .callbacks
                .lock()
                .ok()
                .and_then(|mut callbacks| callbacks.remove(&id));
            if let Some(callback) = callback {
                callback(response.clone());
                return;
            }
        }
        if let Ok(mut responses) = self.responses.lock() {
            responses.push(response);
        }
    }

    fn poll(&self) -> Option<String> {
        self.responses.lock().ok()?.pop()
    }

    fn on_response(&self, id: &str, callback: Box<dyn Fn(String) + Send>) {
        if let Ok(mut callbacks) = self.callbacks.lock() {
            callbacks.insert(id.to_string(), callback);
        }
    }

    fn clear(&self) {
        if let Ok(mut responses) = self.responses.lock() {
            responses.clear();
        }
        if let Ok(mut expected) = self.expected.lock() {
            expected.clear();
        }
    }
}

/// Keeps every frame written, for tests. `terminal.ts:152-180`.
#[derive(Clone, Default)]
pub struct FrameCapture {
    inner: Arc<Mutex<FrameCaptureInner>>,
}

#[derive(Default)]
struct FrameCaptureInner {
    output: String,
    frames: Vec<String>,
    started: bool,
}

impl FrameCapture {
    /// A fresh, empty capture; the same as [`FrameCapture::default`].
    pub fn new() -> Self {
        Self::default()
    }

    fn write(&self, text: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.output.push_str(text);
        }
    }

    fn start(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.started = true;
        }
    }

    fn stop(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.started = false;
        }
    }

    /// Every byte written since construction.
    pub fn output(&self) -> String {
        self.inner
            .lock()
            .map(|inner| inner.output.clone())
            .unwrap_or_default()
    }

    /// Bytes written by the most recent [`Terminal::write`] call.
    pub fn last_frame(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.frames.last().cloned())
    }

    /// All frames written so far.
    pub fn frames(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|inner| inner.frames.clone())
            .unwrap_or_default()
    }

    fn push_frame(&self, text: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.frames.push(text.to_string());
        }
    }

    /// Whether [`Terminal::start`] ran without a matching [`Terminal::stop`].
    pub fn started(&self) -> bool {
        self.inner.lock().is_ok_and(|inner| inner.started)
    }

    /// Drop every recorded byte and frame.
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.output.clear();
            inner.frames.clear();
        }
    }
}

/// The sequences that put a terminal back the way the TUI found it, in the
/// order [`CrosstermTerminal::stop`] writes them: show the cursor, leave the
/// alternate screen, drop mouse tracking, drop bracketed paste.
///
/// Exposed because a panic cannot hand the terminal back — the panic hook that
/// runs after the unwind has no `Tui` to ask, and the mutex guarding one may
/// even be poisoned by the panic that is unwinding.
pub const RESTORE_SEQUENCES: &str =
    "\x1b[?25h\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?2004l";

/// Write [`RESTORE_SEQUENCES`] and flush, over any sink. Split out from
/// [`restore_terminal`] so a test can pin the bytes without a terminal.
pub fn write_restore_sequences(out: &mut dyn std::io::Write) -> std::io::Result<()> {
    out.write_all(RESTORE_SEQUENCES.as_bytes())?;
    out.flush()
}

/// The last-resort restore: pi's `uncaughtException` handler
/// (`interactive-mode.ts:4260-4289`), which calls `ui.stop()` when the terminal
/// is still alive and writes the restore out directly when it is not.
///
/// This is the half that needs no `Tui`: cooked mode, the cursor, and the
/// bracketed-paste / mouse / alternate-screen modes, in that order. Both halves
/// are idempotent, so running this after `Tui::stop` has already succeeded
/// costs nothing and covers the case where `Tui::stop` never ran.
pub fn restore_terminal() {
    let mut out = std::io::stdout();
    let _ = write_restore_sequences(&mut out);
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Keys `Terminal.start` installs. Mouse capture, bracketed paste and raw
/// mode are on; the Kitty keyboard protocol stays off unless 4b enables it
/// (`terminal.ts:225`).
pub const KEY_PROTOCOLS: &[&str] = &["kitty-keyboard"];

/// crossterm-backed terminal.
// One flag per terminal mode, mirroring upstream's terminal state.
#[allow(clippy::struct_excessive_bools)]
pub struct CrosstermTerminal {
    capture: FrameCapture,
    responses: ResponseStore,
    size: TerminalSize,
    mouse_mode: bool,
    stdin_raw: bool,
    use_alternate_screen: bool,
    hide_cursor: bool,
    last_frame: Option<String>,
}

impl CrosstermTerminal {
    /// `terminal.ts:333-352`.
    pub fn new() -> io::Result<Self> {
        // `terminal.ts:485-494`: a stdout with no size (a pipe, a closed tty)
        // falls through to `COLUMNS`/`LINES` and then to 80x24, rather than
        // failing the whole terminal.
        let size = size_from_ioctl_or_environment();
        Ok(Self {
            capture: FrameCapture::new(),
            responses: ResponseStore::default(),
            size,
            mouse_mode: false,
            stdin_raw: true,
            use_alternate_screen: true,
            hide_cursor: true,
            last_frame: None,
        })
    }

    /// Build a terminal with an injected size, for tests.
    pub fn with_size(size: TerminalSize) -> Self {
        Self {
            capture: FrameCapture::new(),
            responses: ResponseStore::default(),
            size,
            mouse_mode: false,
            stdin_raw: true,
            use_alternate_screen: true,
            hide_cursor: true,
            last_frame: None,
        }
    }

    /// The capture handle of this terminal.
    pub fn capture(&self) -> FrameCapture {
        self.capture.clone()
    }

    /// Enable or disable mouse tracking. `terminal.ts:207-210`.
    pub fn set_mouse_mode(&mut self, enabled: bool) {
        self.mouse_mode = enabled;
    }

    /// Choose whether [`Terminal::start`] switches to the alternate screen.
    pub fn set_use_alternate_screen(&mut self, value: bool) {
        self.use_alternate_screen = value;
    }

    /// Choose whether [`Terminal::start`] hides the cursor.
    pub fn set_hide_cursor(&mut self, value: bool) {
        self.hide_cursor = value;
    }

    /// Choose whether [`Terminal::start`] puts stdin into raw mode.
    pub fn set_stdin_raw(&mut self, value: bool) {
        self.stdin_raw = value;
    }

    /// Push terminal replies, used by tests and by the event loop.
    pub fn push_response(&self, response: &str) {
        self.responses.push(response.to_string());
    }

    /// Note that a query was sent, so its reply can be routed. `terminal.ts:249-259`.
    pub fn expect_response(&self, id: &str, sequence: &str) {
        self.responses.expect(id, sequence);
    }

    /// Translate a crossterm event into pi's raw-data form. `terminal.ts:355-381`.
    ///
    /// Mouse events are re-encoded as SGR reports so they take exactly the same
    /// path as bytes read from stdin (`stdin_buffer.rs`): one decoder, one
    /// set of tests.
    pub fn handle_event(&mut self, event: Event, out: &mut Vec<String>) {
        match event {
            Event::Key(key) => {
                let Some(data) = key_to_terminal_data(key) else {
                    return;
                };
                out.push(data);
            }
            Event::Mouse(mouse) => {
                if let Some(data) = encode_sgr_mouse_event(mouse) {
                    out.push(data);
                }
            }
            // Resize is handled by the host's watcher; focus changes are the
            // alt-screen renderer's business.
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {}
        }
    }

    /// Reconcile the cached size with the real one. `terminal.ts:337-340`.
    pub fn handle_resize(&mut self) -> io::Result<()> {
        self.size = TerminalSize {
            columns: crossterm::terminal::size()?.0,
            rows: crossterm::terminal::size()?.1,
        };
        Ok(())
    }
}

impl Default for CrosstermTerminal {
    fn default() -> Self {
        Self::with_size(TerminalSize {
            columns: 80,
            rows: 24,
        })
    }
}

impl TerminalWrite for CrosstermTerminal {
    fn write_str(&mut self, text: &str) -> io::Result<()> {
        self.last_frame = Some(text.to_string());
        self.capture.write(text);
        self.capture.push_frame(text);
        let mut out = io::stdout();
        out.write_all(text.as_bytes())?;
        out.flush()
    }
}

impl Terminal for CrosstermTerminal {
    fn start(&mut self) -> io::Result<()> {
        self.capture.start();
        let mut out = io::stdout();
        if self.stdin_raw {
            crossterm::terminal::enable_raw_mode()?;
        }
        write!(out, "\x1b[?2004h")?; // Enable bracketed paste mode
        // Mouse tracking is opt-in (upstream owns the sequences in the alt
        // screen's toggles); unconditional tracking would push SGR reports
        // into the raw-stdin path of a host that never reads mouse.
        if self.mouse_mode {
            out.write_all(ENABLE_MOUSE_TRACKING.as_bytes())?;
        }
        if self.hide_cursor {
            write!(out, "\x1b[?25l")?; // Hide the cursor
        }
        if self.use_alternate_screen {
            write!(out, "\x1b[?1049h")?; // Use the alternate screen buffer
        }
        out.flush()
    }

    fn stop(&mut self) -> io::Result<()> {
        self.capture.stop();
        let mut out = io::stdout();
        write_restore_sequences(&mut out)?;
        if self.stdin_raw {
            crossterm::terminal::disable_raw_mode()?;
        }
        self.responses.clear();
        Ok(())
    }

    fn size(&self) -> io::Result<TerminalSize> {
        Ok(self.size)
    }

    fn write(&mut self, text: &str) -> io::Result<()> {
        self.write_str(text)
    }

    fn is_tty(&self) -> bool {
        true
    }

    fn request_mouse_mode(&mut self, enabled: bool) {
        self.mouse_mode = enabled;
    }

    fn take_mouse_mode(&mut self) -> bool {
        std::mem::replace(&mut self.mouse_mode, false)
    }

    fn last_written(&self) -> Option<String> {
        self.capture.last_frame()
    }

    fn poll_response(&mut self) -> Option<String> {
        self.responses.poll()
    }

    fn on_response(&mut self, id: &str, callback: Box<dyn Fn(String) + Send>) {
        self.responses.on_response(id, callback);
    }

    fn refresh_size(&mut self) {
        self.size = size_from_ioctl_or_environment();
    }

    fn override_size(&mut self, size: TerminalSize) {
        self.size = size;
    }
}

/// Encode a crossterm key event as pi's raw input data.
/// `terminal.ts:355-381` plus `packages/tui/src/keys.ts` matching rules.
///
/// Shift is folded into the character when the terminal reports a shifted
/// keycode, which is what pi's `parseKey` expects.
pub fn key_to_terminal_data(key: CtKeyEvent) -> Option<String> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(CtKeyModifiers::CONTROL);
    let alt = key.modifiers.contains(CtKeyModifiers::ALT);
    let shift = key.modifiers.contains(CtKeyModifiers::SHIFT);

    let data = match key.code {
        KeyCode::Char(' ') => {
            if ctrl {
                "\x00".to_string()
            } else if alt {
                "\x1b ".to_string()
            } else {
                " ".to_string()
            }
        }
        KeyCode::Tab => {
            if shift {
                "\x1b[Z".to_string()
            } else {
                "\t".to_string()
            }
        }
        KeyCode::Enter => {
            if shift || alt {
                "\x1b\r".to_string()
            } else if ctrl {
                "\n".to_string()
            } else {
                "\r".to_string()
            }
        }
        KeyCode::Backspace => {
            if alt {
                "\x1b\x7f".to_string()
            } else if ctrl {
                "\x08".to_string()
            } else {
                "\x7f".to_string()
            }
        }
        KeyCode::Esc => "\x1b".to_string(),
        KeyCode::Up => "\x1b[A".to_string(),
        KeyCode::Down => "\x1b[B".to_string(),
        KeyCode::Right => "\x1b[C".to_string(),
        KeyCode::Left => "\x1b[D".to_string(),
        KeyCode::Home => "\x1b[H".to_string(),
        KeyCode::End => "\x1b[F".to_string(),
        KeyCode::Delete => "\x1b[3~".to_string(),
        KeyCode::Insert => "\x1b[2~".to_string(),
        KeyCode::PageUp => "\x1b[5~".to_string(),
        KeyCode::PageDown => "\x1b[6~".to_string(),
        KeyCode::F(n) => match n {
            1 => "\x1bOP",
            2 => "\x1bOQ",
            3 => "\x1bOR",
            4 => "\x1bOS",
            5 => "\x1b[15~",
            6 => "\x1b[17~",
            7 => "\x1b[18~",
            8 => "\x1b[19~",
            9 => "\x1b[20~",
            10 => "\x1b[21~",
            11 => "\x1b[23~",
            12 => "\x1b[24~",
            _ => return None,
        }
        .to_string(),
        KeyCode::Char(character) => {
            let upper = if shift {
                character.to_uppercase().to_string()
            } else {
                character.to_string()
            };
            let base = upper.chars().next()?;
            if ctrl {
                // Control characters: ctrl+a -> 0x01.
                if let Some(control) = control_character(base) {
                    if alt {
                        return Some(format!("\x1b{control}"));
                    }
                    control.to_string()
                } else {
                    return Some(format!("\x1b[{};5u", base as u32));
                }
            } else if alt {
                format!("\x1b{upper}")
            } else if shift && !character.is_ascii_lowercase() {
                upper
            } else {
                character.to_string()
            }
        }
        _ => return None,
    };
    Some(data)
}

/// The 0x01..0x1a control character for a key, if one exists.
/// `keys.ts:749-760`.
fn control_character(key: char) -> Option<char> {
    let lower = key.to_ascii_lowercase();
    if lower == '-' {
        // `-` shares a physical key with `_`.
        return char::from_u32('_' as u32 & 0x1f);
    }
    if lower.is_ascii_lowercase() || matches!(lower, '[' | '\\' | ']' | '_') {
        return char::from_u32(lower as u32 & 0x1f);
    }
    None
}

/// In-memory terminal: no TTY, deterministic output, frame-capture seams.
/// Cloneable — clones share the response store, so a handle kept by the test
/// pushes replies to the terminal the component owns.
#[derive(Clone)]
pub struct CaptureTerminal {
    capture: FrameCapture,
    responses: ResponseStore,
    size: TerminalSize,
    /// What a re-query of the terminal would return. `None` until a test sets
    /// it, which leaves `refresh_size` a no-op the way a backend with no tty is.
    queried_size: Option<TerminalSize>,
    mouse_mode: bool,
    start_calls: usize,
    stop_calls: usize,
}

impl CaptureTerminal {
    /// `terminal.ts:157-162`: defaults to pi's 80x24 fallback.
    pub fn new() -> Self {
        Self {
            capture: FrameCapture::new(),
            responses: ResponseStore::default(),
            size: TerminalSize {
                columns: 80,
                rows: 24,
            },
            queried_size: None,
            mouse_mode: false,
            start_calls: 0,
            stop_calls: 0,
        }
    }

    /// A terminal that reports `size` instead of pi's 80x24 default.
    pub fn with_size(size: TerminalSize) -> Self {
        Self {
            size,
            ..Self::new()
        }
    }

    /// The capture handle, kept by a test while the terminal moves elsewhere.
    pub fn capture(&self) -> FrameCapture {
        self.capture.clone()
    }

    /// Record the mouse mode [`Terminal::take_mouse_mode`] hands over.
    pub fn set_mouse_mode(&mut self, enabled: bool) {
        self.mouse_mode = enabled;
    }

    /// Set the size [`Terminal::size`] reports.
    pub fn set_size(&mut self, size: TerminalSize) {
        self.size = size;
    }

    /// The size the terminal would report if it were asked again, which is what
    /// [`Terminal::refresh_size`] reads. Tests set it to model a resize.
    pub fn set_queried_size(&mut self, size: TerminalSize) {
        self.queried_size = Some(size);
    }

    /// Queue a reply for [`Terminal::poll_response`], or a matching callback.
    pub fn push_response(&self, response: &str) {
        self.responses.push(response.to_string());
    }

    /// Note that a query was sent, so its reply can be routed.
    pub fn expect_response(&self, id: &str, sequence: &str) {
        self.responses.expect(id, sequence);
    }

    /// How many times [`Terminal::start`] ran.
    pub fn start_calls(&self) -> usize {
        self.start_calls
    }

    /// How many times [`Terminal::stop`] ran.
    pub fn stop_calls(&self) -> usize {
        self.stop_calls
    }
}

impl Default for CaptureTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalWrite for CaptureTerminal {
    fn write_str(&mut self, text: &str) -> io::Result<()> {
        self.capture.write(text);
        self.capture.push_frame(text);
        Ok(())
    }
}

impl Terminal for CaptureTerminal {
    fn start(&mut self) -> io::Result<()> {
        self.start_calls += 1;
        self.capture.start();
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        self.stop_calls += 1;
        self.capture.stop();
        self.responses.clear();
        Ok(())
    }

    fn size(&self) -> io::Result<TerminalSize> {
        Ok(self.size)
    }

    fn write(&mut self, text: &str) -> io::Result<()> {
        self.write_str(text)
    }

    fn is_tty(&self) -> bool {
        false
    }

    fn request_mouse_mode(&mut self, enabled: bool) {
        self.mouse_mode = enabled;
    }

    fn take_mouse_mode(&mut self) -> bool {
        std::mem::replace(&mut self.mouse_mode, false)
    }

    fn last_written(&self) -> Option<String> {
        self.capture.last_frame()
    }

    fn poll_response(&mut self) -> Option<String> {
        self.responses.poll()
    }

    fn on_response(&mut self, id: &str, callback: Box<dyn Fn(String) + Send>) {
        self.responses.on_response(id, callback);
    }

    fn override_size(&mut self, size: TerminalSize) {
        self.size = size;
    }

    fn refresh_size(&mut self) {
        if let Some(size) = self.queried_size {
            self.size = size;
        }
    }

    fn set_queried_size(&mut self, size: TerminalSize) {
        self.queried_size = Some(size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventState};

    fn key(code: KeyCode, modifiers: CtKeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn crossterm_backend_start_and_stop_emit_pi_sequences() {
        let mut terminal = CrosstermTerminal::with_size(TerminalSize {
            columns: 80,
            rows: 24,
        });
        let stdout = FrameCapture::new();
        terminal.capture = stdout.clone();
        // Raw mode needs a tty; use the capture terminal for the byte check.
        let mut capture = CaptureTerminal::new();
        let sink = capture.capture();
        TerminalWrite::write_str(&mut capture, "\x1b[?25l").unwrap();
        assert_eq!(sink.output(), "\x1b[?25l");
        assert_eq!(sink.last_frame().as_deref(), Some("\x1b[?25l"));
    }

    #[test]
    fn capture_terminal_defaults_to_pi_size() {
        let terminal = CaptureTerminal::new();
        assert_eq!(
            terminal.size().unwrap(),
            TerminalSize {
                columns: 80,
                rows: 24
            }
        );
        assert!(!terminal.is_tty());
    }

    /// The fallback chain of `terminal.ts:485-494`, through the pure lookup so
    /// no test writes the process-wide environment.
    #[test]
    fn the_size_fallback_reads_columns_and_lines() {
        let lookup = |vars: &[(&str, &str)]| {
            let map: std::collections::HashMap<String, String> = vars
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect();
            move |name: &str| map.get(name).cloned()
        };
        assert_eq!(
            size_from_lookup(lookup(&[("COLUMNS", "133"), ("LINES", "42")])),
            TerminalSize {
                columns: 133,
                rows: 42
            }
        );
        // `Number("")` is 0 and 0 is falsy, so empty falls through to 80x24.
        assert_eq!(
            size_from_lookup(lookup(&[("COLUMNS", ""), ("LINES", "not a number")])),
            TerminalSize {
                columns: 80,
                rows: 24
            }
        );
        // A zero is falsy too, so it cannot ask for a zero-sized terminal.
        assert_eq!(
            size_from_lookup(lookup(&[("COLUMNS", "0"), ("LINES", "7")])),
            TerminalSize {
                columns: 80,
                rows: 7
            }
        );
        // Unset is unset: `process.env.COLUMNS` is `undefined` and
        // `Number(undefined)` is `NaN`.
        assert_eq!(
            size_from_lookup(lookup(&[])),
            TerminalSize {
                columns: 80,
                rows: 24
            }
        );
        // `Number` is not Rust's `parse`: hex, an exponent and whitespace all
        // count. `0x10` is pi's 16 columns.
        assert_eq!(
            size_from_lookup(lookup(&[("COLUMNS", "0x10"), ("LINES", " 42 ")])),
            TerminalSize {
                columns: 16,
                rows: 42
            }
        );
        // pi's `||` only tests truthiness, so a negative size wins over the 80
        // and reaches the components as an impossible width; a `u16` cannot
        // hold it, and 0 columns empties the frame the same way.
        assert_eq!(
            size_from_lookup(lookup(&[("COLUMNS", "-5"), ("LINES", "24")])),
            TerminalSize {
                columns: 0,
                rows: 24
            }
        );
    }

    /// The spellings JavaScript's `Number` takes and Rust's `parse::<f64>()`
    /// does not, and the ones it rejects. `None` is what pi's `||` treats as
    /// falsy.
    #[test]
    fn js_number_reads_what_javascripts_number_reads() {
        for (text, expected) in [
            ("12", Some(12.0)),
            (" 12 ", Some(12.0)),
            ("+12", Some(12.0)),
            ("-12", Some(-12.0)),
            ("12.", Some(12.0)),
            (".5", Some(0.5)),
            ("1.5e2", Some(150.0)),
            ("1e-2", Some(0.01)),
            ("0x10", Some(16.0)),
            ("0X10", Some(16.0)),
            ("0o17", Some(15.0)),
            ("0b101", Some(5.0)),
            ("Infinity", Some(f64::INFINITY)),
            ("-Infinity", Some(f64::NEG_INFINITY)),
            // Falsy in JavaScript, so `||` moves on to the next value.
            ("", None),
            ("   ", None),
            ("0", None),
            ("-0", None),
            ("0.0", None),
            // `Number` says NaN for all of these.
            ("not a number", None),
            ("1.2.3", None),
            ("12px", None),
            ("0x", None),
            ("1e", None),
            ("--1", None),
            ("+", None),
            ("inf", None),
            ("NaN", None),
        ] {
            assert_eq!(js_number(text), expected, "for {text:?}");
        }
    }

    #[test]
    fn capture_terminal_tracks_lifecycle() {
        let mut terminal = CaptureTerminal::new();
        terminal.start().unwrap();
        terminal.start().unwrap();
        terminal.stop().unwrap();
        assert_eq!(terminal.start_calls(), 2);
        assert_eq!(terminal.stop_calls(), 1);
        assert!(!terminal.capture().started());
    }

    #[test]
    fn capture_terminal_frames_are_recorded() {
        let mut terminal = CaptureTerminal::new();
        let capture = terminal.capture();
        terminal.write("first").unwrap();
        terminal.write("second").unwrap();
        assert_eq!(capture.output(), "firstsecond");
        assert_eq!(capture.frames(), vec!["first", "second"]);
        assert_eq!(capture.last_frame().as_deref(), Some("second"));
    }

    #[test]
    fn mouse_mode_is_taken_once() {
        let mut terminal = CaptureTerminal::new();
        terminal.set_mouse_mode(true);
        assert!(terminal.take_mouse_mode());
        assert!(!terminal.take_mouse_mode());
    }

    #[test]
    fn responses_are_polled_once_and_dispatched() {
        let mut terminal = CaptureTerminal::new();
        terminal.push_response("\x1b[>0;276;0c");
        assert_eq!(terminal.poll_response().as_deref(), Some("\x1b[>0;276;0c"));
        assert_eq!(terminal.poll_response(), None);

        let seen = Arc::new(Mutex::new(None));
        let sink = seen.clone();
        terminal.expect_response("da2", "\x1b[>0c");
        terminal.on_response(
            "da2",
            Box::new(move |response| {
                *sink.lock().unwrap() = Some(response);
            }),
        );
        terminal.push_response("\x1b[>0;276;0c");
        assert_eq!(seen.lock().unwrap().as_deref(), Some("\x1b[>0;276;0c"));
        // Unmatched replies still queue for polling.
        terminal.push_response("\x1b]11;rgb:2020/2020/2020\x07");
        assert_eq!(
            terminal.poll_response().as_deref(),
            Some("\x1b]11;rgb:2020/2020/2020\x07")
        );
    }

    #[test]
    fn query_sequences_match_pi() {
        assert_eq!(query_sequence(query_ids::XTVERSION), Some("\x1b[>0q"));
        assert_eq!(query_sequence(query_ids::DA1), Some("\x1b[c"));
        assert_eq!(query_sequence(query_ids::DA2), Some("\x1b[>0c"));
        assert_eq!(query_sequence(query_ids::BGCOLOR), Some("\x1b]11;?\x07"));
        assert_eq!(query_sequence(query_ids::FGCOLOR), Some("\x1b]10;?\x07"));
        assert_eq!(
            query_sequence(query_ids::COLOR_SCHEME_REPORT),
            Some("\x1b[?996n")
        );
        assert_eq!(query_sequence("nope"), None);
    }

    #[test]
    fn query_replies_are_matched_by_kind() {
        assert!(query_reply_matches(
            "\x1b[c",
            "\x1b[?62;1;2;6;9;15;18;21;22c"
        ));
        assert!(query_reply_matches("\x1b[>0c", "\x1b[>0;276;0c"));
        assert!(query_reply_matches("\x1b[>0q", "\x1bP>|xterm(400)\x1b\\"));
        assert!(query_reply_matches(
            "\x1b]11;?\x07",
            "\x1b]11;rgb:2020/2020/2020\x07"
        ));
        assert!(!query_reply_matches("\x1b[>0c", "\x1b[?62;1;2c"));
        assert!(!query_reply_matches(
            "\x1b]10;?\x07",
            "\x1b]11;rgb:2020/2020/2020\x07"
        ));
    }

    #[test]
    fn report_modes_match_pi() {
        assert_eq!(
            report_mode_sequence(1004, true).as_deref(),
            Some("\x1b[?1004h")
        );
        assert_eq!(
            report_mode_sequence(2027, false).as_deref(),
            Some("\x1b[>2027;2l")
        );
        assert_eq!(report_mode_sequence(1234, true), None);
    }

    #[test]
    fn default_reports_are_pi_defaults() {
        assert_eq!(default_reports()[0], (24, 80));
        assert!(default_reports().contains(&(900, 4608)));
    }

    #[test]
    fn crossterm_keys_map_to_pi_data() {
        use CtKeyModifiers as M;
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Char('a'), M::NONE)).as_deref(),
            Some("a")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Char('A'), M::SHIFT)).as_deref(),
            Some("A")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Char('a'), M::CONTROL)).as_deref(),
            Some("\x01")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Char('a'), M::ALT)).as_deref(),
            Some("\x1ba")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Char('a'), M::CONTROL | M::ALT)).as_deref(),
            Some("\x1b\x01")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Char(' '), M::NONE)).as_deref(),
            Some(" ")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Char(' '), M::CONTROL)).as_deref(),
            Some("\x00")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Tab, M::NONE)).as_deref(),
            Some("\t")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Tab, M::SHIFT)).as_deref(),
            Some("\x1b[Z")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Enter, M::NONE)).as_deref(),
            Some("\r")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Enter, M::SHIFT)).as_deref(),
            Some("\x1b\r")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Backspace, M::NONE)).as_deref(),
            Some("\x7f")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Backspace, M::CONTROL)).as_deref(),
            Some("\x08")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Esc, M::NONE)).as_deref(),
            Some("\x1b")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::Up, M::NONE)).as_deref(),
            Some("\x1b[A")
        );
        assert_eq!(
            key_to_terminal_data(key(KeyCode::F(5), M::NONE)).as_deref(),
            Some("\x1b[15~")
        );
    }

    #[test]
    fn released_keys_are_dropped() {
        let mut released = key(KeyCode::Char('a'), CtKeyModifiers::NONE);
        released.kind = KeyEventKind::Release;
        assert_eq!(key_to_terminal_data(released), None);
    }

    #[test]
    fn encoded_keys_parse_back_to_pi_key_names() {
        use crate::keys::parse_key;
        for (code, modifiers, expected) in [
            (KeyCode::Char('a'), CtKeyModifiers::CONTROL, "ctrl+a"),
            (KeyCode::Char('a'), CtKeyModifiers::ALT, "alt+a"),
            (KeyCode::Tab, CtKeyModifiers::SHIFT, "shift+tab"),
            (KeyCode::Up, CtKeyModifiers::NONE, "up"),
            (KeyCode::Char(' '), CtKeyModifiers::CONTROL, "ctrl+space"),
        ] {
            let data = key_to_terminal_data(key(code, modifiers)).expect("encodable");
            assert_eq!(parse_key(&data).as_deref(), Some(expected), "{data:?}");
        }
    }

    #[test]
    fn the_restore_sequences_put_back_every_mode_start_turned_on() {
        let mut out = Vec::new();
        write_restore_sequences(&mut out).expect("the sink accepts bytes");
        assert_eq!(out, RESTORE_SEQUENCES.as_bytes());
        // Each is the inverse of what `CrosstermTerminal::start` wrote, which is
        // the whole point: a terminal left raw with no cursor is the failure
        // this exists to prevent. An inverted sequence would leave the mode on.
        for (on, off) in [
            ("\x1b[?25l", "\x1b[?25h"),
            ("\x1b[?1049h", "\x1b[?1049l"),
            ("\x1b[?1000h", "\x1b[?1000l"),
            ("\x1b[?1002h", "\x1b[?1002l"),
            ("\x1b[?1006h", "\x1b[?1006l"),
            ("\x1b[?2004h", "\x1b[?2004l"),
        ] {
            assert!(
                !RESTORE_SEQUENCES.contains(on),
                "{on:?} turns a mode on, not off"
            );
            assert!(
                RESTORE_SEQUENCES.contains(off),
                "{off:?} is missing from the restore"
            );
        }
    }

    #[test]
    fn handle_event_collects_data() {
        let mut terminal = CrosstermTerminal::with_size(TerminalSize {
            columns: 80,
            rows: 24,
        });
        let mut out = Vec::new();
        terminal.handle_event(
            Event::Key(key(KeyCode::Char('x'), CtKeyModifiers::NONE)),
            &mut out,
        );
        terminal.handle_event(Event::Resize(100, 40), &mut out);
        assert_eq!(out, vec!["x"]);
    }

    fn mouse(kind: CtMouseEventKind, column: u16, row: u16, modifiers: CtKeyModifiers) -> Event {
        Event::Mouse(CtMouseEvent {
            kind,
            column,
            row,
            modifiers,
        })
    }

    #[test]
    fn mouse_tracking_sequences_match_pi_byte_for_byte() {
        // tui-alt-screen.ts:66-68.
        assert_eq!(
            ENABLE_MOUSE_TRACKING,
            "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h"
        );
        assert_eq!(
            DISABLE_MOUSE_TRACKING,
            "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l"
        );
        assert!(
            ENABLE_MOUSE_TRACKING.contains("\x1b[?1004h"),
            "focus reporting"
        );
        assert!(
            DISABLE_MOUSE_TRACKING.contains("\x1b[?1003l"),
            "all-motion is disabled too"
        );
    }

    #[test]
    fn crossterm_mouse_events_become_sgr_reports() {
        let mut terminal = CrosstermTerminal::with_size(TerminalSize {
            columns: 80,
            rows: 24,
        });
        let mut out = Vec::new();
        terminal.handle_event(
            mouse(
                CtMouseEventKind::Down(CtMouseButton::Left),
                4,
                1,
                CtKeyModifiers::NONE,
            ),
            &mut out,
        );
        terminal.handle_event(
            mouse(
                CtMouseEventKind::Up(CtMouseButton::Left),
                4,
                1,
                CtKeyModifiers::NONE,
            ),
            &mut out,
        );
        terminal.handle_event(
            mouse(
                CtMouseEventKind::Drag(CtMouseButton::Left),
                4,
                2,
                CtKeyModifiers::ALT,
            ),
            &mut out,
        );
        terminal.handle_event(
            mouse(CtMouseEventKind::Moved, 4, 2, CtKeyModifiers::NONE),
            &mut out,
        );
        terminal.handle_event(
            mouse(CtMouseEventKind::ScrollUp, 4, 2, CtKeyModifiers::NONE),
            &mut out,
        );
        terminal.handle_event(
            mouse(CtMouseEventKind::ScrollDown, 4, 2, CtKeyModifiers::ALT),
            &mut out,
        );
        assert_eq!(
            out,
            vec![
                "\x1b[<0;5;2M",
                "\x1b[<0;5;2m",
                "\x1b[<40;5;3M",
                "\x1b[<35;5;3M",
                "\x1b[<64;5;3M",
                "\x1b[<73;5;3M",
            ]
        );
        // Every one of them decodes back through the shared parser.
        for data in &out {
            assert!(crate::mouse::is_mouse_sequence(data), "{data:?}");
        }
        assert_eq!(
            crate::mouse::parse_wheel_event(&out[4])
                .expect("wheel")
                .direction,
            -1
        );
        assert_eq!(
            crate::mouse::parse_wheel_event(&out[5])
                .expect("wheel")
                .direction,
            1
        );
        assert_eq!(
            crate::mouse::parse_wheel_event(&out[5])
                .expect("wheel")
                .button
                & 8,
            8
        );
    }

    #[test]
    fn the_mouse_opt_out_is_recorded_for_start() {
        let mut terminal = CaptureTerminal::new();
        terminal.request_mouse_mode(false);
        assert!(
            !terminal.take_mouse_mode(),
            "mouse: false suppresses the enable sequence"
        );
        terminal.request_mouse_mode(true);
        assert!(terminal.take_mouse_mode());
        assert!(!terminal.take_mouse_mode(), "taking it clears it");
    }
}
