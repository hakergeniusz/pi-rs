//! `Input`: single-line text input with horizontal scrolling.
//! `packages/tui/src/components/input.ts` (494 lines).
//!
//! pi-tui had no `Input` before 5c: the search overlay (`alt-screen-search.ts`)
//! and the settings submenu both wrap one. Keys come from the same `tui.*`
//! defaults the editor uses, matched with [`crate::keys::matches_key`].

use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::{slice_by_column, truncate_to_width, visible_width};
use crate::component::{Component, ComponentMutation, Focusable};
use crate::keys::{decode_kitty_printable, matches_key};
use crate::kill_ring::KillRing;
use crate::mouse::{MouseButton, MouseEvent, MouseEventKind, MouseEventResult};
use crate::undo_stack::UndoStack;
use crate::word_navigation::{find_word_backward, find_word_forward, is_whitespace_char};

/// `CURSOR_MARKER`, `tui.ts:191`.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";
/// `ESC[7m` reverse video opens the fake cursor. `input.ts:487`.
const CURSOR_ON: &str = "\x1b[7m";
/// `ESC[27m` normal video closes it. `input.ts:487`.
const CURSOR_OFF: &str = "\x1b[27m";

/// `InputOptions`, `input.ts:19-23`.
#[derive(Clone)]
pub struct InputOptions {
    pub prompt: String,
    pub placeholder: String,
    pub placeholder_style: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl Default for InputOptions {
    /// `input.ts:45-48`.
    fn default() -> Self {
        Self {
            prompt: "> ".to_string(),
            placeholder: String::new(),
            placeholder_style: Arc::new(|text: &str| text.to_string()),
        }
    }
}

/// `InputState`, `input.ts:13-16`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InputState {
    value: String,
    cursor: usize,
}

/// The edit that produced the current state, for undo coalescing.
/// `input.ts:41`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// What a key asked the host to do. pi fires callbacks; the port returns the
/// action so the host can react (`input.ts:36-37`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Submitted(String),
    Escaped,
    None,
}

/// Single-line input. `input.ts:28-...`.
pub struct Input {
    value: String,
    /// Byte offset into `value`.
    cursor: usize,
    prompt: String,
    placeholder: String,
    placeholder_style: Arc<dyn Fn(&str) -> String + Send + Sync>,
    /// First visible column of the value, set by `render` and read back by the
    /// mouse handler. `input.ts:36`.
    rendered_start_column: std::cell::Cell<usize>,
    focused: bool,
    paste_buffer: String,
    in_paste: bool,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    undo_stack: UndoStack<InputState>,
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Input")
            .field("value", &self.value)
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

impl Input {
    /// `input.ts:45-49`.
    pub fn new(options: InputOptions) -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            prompt: options.prompt,
            placeholder: options.placeholder,
            placeholder_style: options.placeholder_style,
            rendered_start_column: std::cell::Cell::new(0),
            focused: false,
            paste_buffer: String::new(),
            in_paste: false,
            kill_ring: KillRing::new(),
            last_action: None,
            undo_stack: UndoStack::new(),
        }
    }

    pub fn with_prompt(prompt: impl Into<String>) -> Self {
        Self::new(InputOptions {
            prompt: prompt.into(),
            ..InputOptions::default()
        })
    }

    /// `input.ts:51-53`.
    #[must_use]
    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// `input.ts:57-60`.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// `input.ts:62-65`.
    pub fn set_value(&mut self, value: impl Into<String>) {
        let value = value.into();
        let length = value.len();
        self.value = value;
        self.cursor = self.cursor.min(length);
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn push_undo(&mut self) {
        self.undo_stack.push(InputState {
            value: self.value.clone(),
            cursor: self.cursor,
        });
    }

    /// `input.ts:379-385`.
    fn undo(&mut self) {
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.value = snapshot.value;
        self.cursor = snapshot.cursor;
        self.last_action = None;
    }

    /// `insertCharacter`, `input.ts:247-257`.
    fn insert_character(&mut self, text: &str) {
        let first = text.chars().next();
        if first.is_none_or(is_whitespace_char) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// `handleBackspace`, `input.ts:259-271`.
    fn backspace(&mut self) {
        self.last_action = None;
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let previous = grapheme_start(&self.value, self.cursor);
        self.value.replace_range(previous..self.cursor, "");
        self.cursor = previous;
    }

    /// `handleForwardDelete`, `input.ts:273-283`.
    fn forward_delete(&mut self) {
        self.last_action = None;
        if self.cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let next = grapheme_end(&self.value, self.cursor);
        self.value.replace_range(self.cursor..next, "");
    }

    /// `deleteToLineStart`, `input.ts:285-294`.
    fn delete_to_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let killed = self.value[..self.cursor].to_string();
        self.kill_ring
            .push(&killed, true, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value.replace_range(..self.cursor, "");
        self.cursor = 0;
    }

    /// `deleteToLineEnd`, `input.ts:296-306`.
    fn delete_to_line_end(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let killed = self.value[self.cursor..].to_string();
        self.kill_ring
            .push(&killed, false, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value.truncate(self.cursor);
    }

    fn move_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_backward(&self.value, self.cursor, None);
    }

    fn move_word_forwards(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_forward(&self.value, self.cursor, None);
    }

    /// `deleteWordBackwards`, `input.ts:308-327`.
    fn delete_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let old_cursor = self.cursor;
        self.move_word_backwards();
        let delete_from = self.cursor;
        self.cursor = old_cursor;
        let deleted = self.value[delete_from..self.cursor].to_string();
        self.kill_ring.push(&deleted, true, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.value.replace_range(delete_from..self.cursor, "");
        self.cursor = delete_from;
    }

    /// `deleteWordForward`, `input.ts:329-347`.
    fn delete_word_forward(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let old_cursor = self.cursor;
        self.move_word_forwards();
        let delete_to = self.cursor;
        self.cursor = old_cursor;
        let deleted = self.value[self.cursor..delete_to].to_string();
        self.kill_ring.push(&deleted, false, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.value.replace_range(self.cursor..delete_to, "");
    }

    /// `yank`, `input.ts:349-359`.
    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        self.push_undo();
        let length = text.len();
        self.value.insert_str(self.cursor, &text);
        self.cursor += length;
        self.last_action = Some(LastAction::Yank);
    }

    /// `yankPop`, `input.ts:361-377`.
    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();
        let previous = self
            .kill_ring
            .peek()
            .map(str::to_string)
            .unwrap_or_default();
        let previous_len = previous.len();
        self.value
            .replace_range(self.cursor.saturating_sub(previous_len)..self.cursor, "");
        self.cursor = self.cursor.saturating_sub(previous_len);
        self.kill_ring.rotate();
        let text = self
            .kill_ring
            .peek()
            .map(str::to_string)
            .unwrap_or_default();
        let length = text.len();
        self.value.insert_str(self.cursor, &text);
        self.cursor += length;
        self.last_action = Some(LastAction::Yank);
    }

    /// `handlePaste`, `input.ts:387-398`.
    fn handle_paste(&mut self, pasted: &str) {
        self.last_action = None;
        self.push_undo();
        let clean: String = pasted
            .replace("\r\n", "")
            .replace(['\r', '\n'], "")
            .replace('\t', "    ");
        self.value.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    /// `handleInput`, `input.ts:67-231`.
    pub fn handle_key(&mut self, data: &str) -> InputAction {
        if data.contains("\x1b[200~") || self.in_paste {
            return self.handle_paste_stream(data);
        }

        if select_keys_cancel(data) {
            return InputAction::Escaped;
        }
        if matches_key(data, "ctrl+-") {
            self.undo();
            return InputAction::None;
        }
        if matches_key(data, "enter") || data == "\n" {
            return InputAction::Submitted(self.value.clone());
        }
        if matches_key(data, "left") {
            self.last_action = None;
            if self.cursor > 0 {
                self.cursor = grapheme_start(&self.value, self.cursor);
            }
            return InputAction::None;
        }
        if matches_key(data, "right") {
            self.last_action = None;
            if self.cursor < self.value.len() {
                self.cursor = grapheme_end(&self.value, self.cursor);
            }
            return InputAction::None;
        }
        if matches_key(data, "home") {
            self.last_action = None;
            self.cursor = 0;
            return InputAction::None;
        }
        if matches_key(data, "end") {
            self.last_action = None;
            self.cursor = self.value.len();
            return InputAction::None;
        }
        if matches_key(data, "alt+left") || matches_key(data, "ctrl+left") {
            self.move_word_backwards();
            return InputAction::None;
        }
        if matches_key(data, "alt+right") || matches_key(data, "ctrl+right") {
            self.move_word_forwards();
            return InputAction::None;
        }
        if matches_key(data, "backspace") {
            self.backspace();
            return InputAction::None;
        }
        if matches_key(data, "delete") {
            self.forward_delete();
            return InputAction::None;
        }
        if matches_key(data, "ctrl+w") || matches_key(data, "alt+backspace") {
            self.delete_word_backwards();
            return InputAction::None;
        }
        if matches_key(data, "alt+d") || matches_key(data, "alt+delete") {
            self.delete_word_forward();
            return InputAction::None;
        }
        if matches_key(data, "ctrl+u") {
            self.delete_to_line_start();
            return InputAction::None;
        }
        if matches_key(data, "ctrl+k") {
            self.delete_to_line_end();
            return InputAction::None;
        }
        if matches_key(data, "ctrl+y") {
            self.yank();
            return InputAction::None;
        }
        if matches_key(data, "alt+y") {
            self.yank_pop();
            return InputAction::None;
        }

        // Kitty CSI-u printable (`input.ts:207-211`): decode before the
        // control-character check, because the sequence itself contains ESC.
        if let Some(printed) = decode_kitty_printable(data) {
            self.insert_character(&printed.to_string());
            return InputAction::None;
        }
        if !has_control_chars(data) {
            self.insert_character(data);
        }
        InputAction::None
    }

    /// Bracketed paste streaming. `input.ts:68-100`.
    fn handle_paste_stream(&mut self, data: &str) -> InputAction {
        if !self.in_paste {
            self.in_paste = true;
            self.paste_buffer.clear();
        }
        let data = data.replace("\x1b[200~", "");
        self.paste_buffer.push_str(&data);
        let Some(end) = self.paste_buffer.find("\x1b[201~") else {
            return InputAction::None;
        };
        let pasted = self.paste_buffer[..end].to_string();
        let remaining = self.paste_buffer[end + 6..].to_string();
        self.paste_buffer.clear();
        self.in_paste = false;
        self.handle_paste(&pasted);
        if remaining.is_empty() {
            return InputAction::None;
        }
        self.handle_key(&remaining)
    }
}

/// `tui.select.cancel` default keys. `keybindings.ts:155-159`.
fn select_keys_cancel(data: &str) -> bool {
    matches_key(data, "escape") || matches_key(data, "ctrl+c")
}

/// `hasControlChars`, `input.ts:213-217`.
fn has_control_chars(data: &str) -> bool {
    data.chars().any(|c| {
        let code = c as u32;
        code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
    })
}

fn grapheme_start(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn grapheme_end(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .grapheme_indices(true)
        .next()
        .map_or(value.len(), |(index, grapheme)| {
            cursor + index + grapheme.len()
        })
}

impl Focusable for Input {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl Component for Input {
    /// `input.ts:412-493`. Returns one line: the prompt, the value with a
    /// reverse-video cell at the cursor, padded to the available width.
    fn render(&self, width: usize) -> Vec<String> {
        let prompt_width = visible_width(&self.prompt);
        let available_width = (width as i64 - prompt_width as i64).max(0) as usize;
        if available_width == 0 {
            return vec![truncate_to_width(&self.prompt, width, "", false)];
        }

        if self.value.is_empty() && !self.placeholder.is_empty() {
            let placeholder = truncate_to_width(&self.placeholder, available_width, "", false);
            let at_cursor = placeholder
                .graphemes(true)
                .next()
                .unwrap_or(" ")
                .to_string();
            let after_cursor = &placeholder[at_cursor.len()..];
            let marker = if self.focused { CURSOR_MARKER } else { "" };
            let cursor_cell = format!(
                "{CURSOR_ON}{}\x1b[27m",
                (self.placeholder_style)(&at_cursor)
            );
            let text = format!(
                "{marker}{cursor_cell}{}",
                (self.placeholder_style)(after_cursor)
            );
            let padding = available_width.saturating_sub(visible_width(&text));
            return vec![format!("{}{}{}", self.prompt, text, " ".repeat(padding))];
        }

        let visible_text;
        let cursor_display;
        let mut rendered_start_column = 0usize;
        let total_width = visible_width(&self.value);
        if total_width < available_width {
            visible_text = self.value.clone();
            cursor_display = self.cursor;
        } else {
            let scroll_width = if self.cursor == self.value.len() {
                available_width.saturating_sub(1)
            } else {
                available_width
            };
            let cursor_col = visible_width(&self.value[..self.cursor]);
            if scroll_width == 0 {
                return vec![format!("{}{}", self.prompt, " ".repeat(available_width))];
            }
            let half_width = scroll_width / 2;
            let start_col = if cursor_col < half_width {
                0
            } else if cursor_col > total_width.saturating_sub(half_width) {
                total_width.saturating_sub(scroll_width)
            } else {
                cursor_col.saturating_sub(half_width)
            };
            rendered_start_column = start_col;
            visible_text = slice_by_column(&self.value, start_col, scroll_width, true);
            let before_cursor = slice_by_column(
                &self.value,
                start_col,
                cursor_col.saturating_sub(start_col),
                true,
            );
            cursor_display = before_cursor.len();
        }
        self.rendered_start_column.set(rendered_start_column);

        let at_cursor = visible_text[cursor_display.min(visible_text.len())..]
            .graphemes(true)
            .next()
            .map_or_else(|| " ".to_string(), str::to_string);
        let before_cursor = &visible_text[..cursor_display.min(visible_text.len())];
        let after_cursor =
            &visible_text[(cursor_display + at_cursor.len()).min(visible_text.len())..];
        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let text =
            format!("{before_cursor}{marker}{CURSOR_ON}{at_cursor}{CURSOR_OFF}{after_cursor}");
        let padding = available_width.saturating_sub(visible_width(&text));
        vec![format!("{}{}{}", self.prompt, text, " ".repeat(padding))]
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        let action = self.handle_key(data);
        Some(ComponentMutation {
            consume: action != InputAction::None,
        })
    }

    /// `Input.handleMouse`, `input.ts:229-245`. Click-only: the cursor lands on
    /// the grapheme under the pointer, one grapheme wide so wide characters
    /// are never split.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        if event.kind != MouseEventKind::Press || event.button != MouseButton::Left || event.y != 0
        {
            return None;
        }
        // The prompt is two cells wide (`input.ts:231` hard-codes the offset).
        let visible_column = (event.x - 2).max(0) as usize;
        let target_column = self.rendered_start_column.get() + visible_column;
        let mut current_column = 0usize;
        self.cursor = self.value.len();
        for (index, grapheme) in self.value.grapheme_indices(true) {
            let next_column = current_column + visible_width(grapheme);
            if target_column < next_column {
                self.cursor = index;
                break;
            }
            current_column = next_column;
        }
        self.last_action = None;
        Some(MouseEventResult::focus())
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_terminal_sequences;

    /// pi's `setValue` keeps the cursor where it was (clamped); the tests below
    /// always want it at the end, like a freshly typed field.
    fn input(value: &str) -> Input {
        let mut input = Input::with_prompt("> ");
        input.set_value(value);
        input.handle_key("\x1b[F");
        input
    }

    #[test]
    fn renders_the_prompt_and_value() {
        let rendered = strip_terminal_sequences(&input("hello").render(20)[0]);
        assert!(rendered.starts_with("> "));
        assert!(rendered.contains("hello"));
        assert_eq!(visible_width(&input("hello").render(20)[0]), 20);
    }

    #[test]
    fn shows_the_placeholder_when_empty() {
        let mut field = Input::with_prompt("> ").with_placeholder("Find in transcript");
        let rendered = strip_terminal_sequences(&field.render(40)[0]);
        assert!(rendered.contains("Find in transcript"));
        field.set_value("x");
        assert!(!strip_terminal_sequences(&field.render(40)[0]).contains("Find"));
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut field = input("hello");
        field.handle_key("\x1b[H");
        field.handle_key("X");
        assert_eq!(field.value(), "Xhello");
        field.handle_key("\x1b[C");
        field.handle_key("\x1b[C");
        assert_eq!(field.cursor(), 3);
        field.handle_key("!");
        assert_eq!(field.value(), "Xhe!llo");
    }

    #[test]
    fn backspace_removes_a_whole_grapheme() {
        let mut field = input("hé");
        assert_eq!(field.cursor(), 3);
        field.handle_key("\x7f");
        assert_eq!(field.value(), "h", "the two-byte grapheme goes as one unit");
        field.handle_key("\x7f");
        assert_eq!(field.value(), "");
        field.handle_key("\x7f");
        assert_eq!(field.value(), "", "backspace at the start is a no-op");
    }

    #[test]
    fn submit_and_cancel_report_the_action() {
        let mut field = input("run");
        assert_eq!(
            field.handle_key("\r"),
            InputAction::Submitted("run".to_string())
        );
        assert_eq!(field.handle_key("\x1b"), InputAction::Escaped);
        assert_eq!(field.handle_key("\x03"), InputAction::Escaped);
    }

    #[test]
    fn bracketed_paste_inserts_and_strips_newlines() {
        let mut field = input("");
        field.handle_key("\x1b[200~a\r\nb\tc\x1b[201~");
        assert_eq!(field.value(), "ab    c");
    }

    #[test]
    fn kill_and_yank_round_trip() {
        let mut field = input("hello world");
        field.handle_key("\x15");
        assert_eq!(field.value(), "");
        field.handle_key("\x19");
        assert_eq!(field.value(), "hello world");
    }

    #[test]
    fn yank_pop_rotates_the_kill_ring() {
        let mut field = input("");
        field.handle_key("ab");
        field.handle_key("\x15");
        field.handle_key("cd");
        field.handle_key("\x15");
        assert_eq!(field.value(), "");
        field.handle_key("\x19");
        assert_eq!(field.value(), "cd");
        field.handle_key("\x1by");
        assert_eq!(field.value(), "ab", "yank-pop pulls the previous kill");
    }

    #[test]
    fn consecutive_kills_accumulate_into_one_entry() {
        let mut field = input("hello");
        field.handle_key("\x15");
        assert_eq!(field.value(), "");
        field.handle_key("\x15");
        field.handle_key("\x15");
        assert_eq!(field.value(), "", "nothing left to kill");
        field.handle_key("\x19");
        assert_eq!(field.value(), "hello", "the accumulated kill is one entry");
    }

    #[test]
    fn undo_restores_the_previous_undo_unit() {
        // Consecutive typing coalesces into one undo unit (`input.ts:249-252`),
        // so a single undo takes "ab" back to "".
        let mut field = input("");
        field.handle_key("a");
        field.handle_key("b");
        assert_eq!(field.value(), "ab");
        field.handle_key("\x1f");
        assert_eq!(field.value(), "");
        field.handle_key("a");
        field.handle_key(" ");
        field.handle_key("\x1f");
        assert_eq!(field.value(), "a");
    }

    #[test]
    fn mouse_press_places_the_cursor() {
        // mouse-components.test.ts:110-118 — "hello", press at x=4.
        let mut field = input("hello");
        let rendered = field.render(20);
        assert_eq!(rendered.len(), 1);
        let result = field
            .handle_mouse(&MouseEvent::new(MouseEventKind::Press, 0, 4, 0, 20, 1))
            .expect("handled");
        assert_eq!(result.handled, Some(true));
        assert_eq!(result.focus, Some(true));
        field.handle_key("X");
        assert_eq!(field.value(), "heXllo");
    }

    #[test]
    fn mouse_ignores_non_press_events() {
        let mut field = input("hello");
        for kind in [
            MouseEventKind::Move,
            MouseEventKind::Release,
            MouseEventKind::Click,
            MouseEventKind::Drag,
        ] {
            assert!(
                field
                    .handle_mouse(&MouseEvent::new(kind, 0, 4, 0, 20, 1))
                    .is_none(),
                "{kind:?}"
            );
        }
        let right = MouseEvent::new(MouseEventKind::Press, 2, 4, 0, 20, 1);
        assert!(field.handle_mouse(&right).is_none());
    }

    #[test]
    fn mouse_click_beyond_the_text_lands_at_the_end() {
        let mut field = input("hi");
        field
            .handle_mouse(&MouseEvent::new(MouseEventKind::Press, 0, 15, 0, 20, 1))
            .expect("handled");
        assert_eq!(field.cursor(), 2);
    }

    #[test]
    fn mouse_press_on_a_wide_grapheme_lands_on_its_start() {
        let mut field = input("日本");
        field.render(20);
        // Cell 1 of the first wide grapheme: the cursor must not land inside it.
        field
            .handle_mouse(&MouseEvent::new(MouseEventKind::Press, 0, 3, 0, 20, 1))
            .expect("handled");
        assert_eq!(field.cursor(), 0);
        field.handle_key("X");
        assert_eq!(field.value(), "X日本");
    }
}
