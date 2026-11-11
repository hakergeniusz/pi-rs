//! `Input`: pi's single-line text input.
//! `packages/tui/src/components/input.ts`.
//!
//! This is the component `ExtensionInputComponent` puts on screen for
//! `ctx.ui.input` (`modes/interactive/components/extension-input.ts:55`), and
//! the one `McpManagerView.redirectUrl` types a pasted sign-in redirect into
//! (`extensions/mcp/ui.ts:161`). It is not the editor: it is one line, it has
//! no history, and its prompt is `> `.
//!
//! The key dispatch is pi's order (`input.ts:104-211`), one action per key,
//! first match wins — which is why `escape` and `ctrl+c` cancel here, and why
//! `ExtensionInputComponent` intercepts `tui.select.confirm`/`tui.select.cancel`
//! *before* handing anything else to this component
//! (`extension-input.ts:82-90`): its own `ctrl+c` is `tui.input.copy`, a
//! different action that never reaches the host's dialog.

use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::{CURSOR_MARKER, slice_by_column, visible_width};
use crate::component::Component;
use crate::keybindings::{action, matches};
use crate::keys::decode_kitty_printable;
use crate::kill_ring::KillRing;
use crate::undo_stack::UndoStack;
use crate::word_navigation::{find_word_backward, find_word_forward, is_whitespace_char};

/// What one key did. pi's `onSubmit` / `onEscape` callbacks
/// (`input.ts:28-29`), as a value so a host can drive the component without
/// owning a closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// `tui.input.submit`, or a bare `\n` (`input.ts:115-118`).
    Submit(String),
    /// `tui.select.cancel` (`input.ts:106-109`).
    Cancel,
    /// The key edited the value, or was not one this component binds.
    None,
}

/// pi's snapshot: the value and the cursor (`input.ts:24-27`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct InputState {
    value: String,
    /// A character index, the same unit `word_navigation` counts in.
    cursor: usize,
}

/// Which action ran last, for undo coalescing and kill accumulation
/// (`input.ts:32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// The bracketed-paste markers (`input.ts:73-104`).
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

/// `Input`. `input.ts:36-227`.
pub struct Input {
    value: String,
    cursor: usize,
    prompt: String,
    placeholder: String,
    placeholder_style: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
    focused: bool,
    /// `input.ts:32-33`.
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    undo: UndoStack<InputState>,
    /// `input.ts:30`, the column the visible window starts at; a click maps
    /// through it.
    rendered_start_column: usize,
    paste_buffer: String,
    in_paste: bool,
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Input")
            .field("value", &self.value)
            .field("cursor", &self.cursor)
            .field("prompt", &self.prompt)
            .field("placeholder", &self.placeholder)
            .finish_non_exhaustive()
    }
}

impl Input {
    /// `input.ts:49-52`: the prompt is `> ` and there is no placeholder, which is
    /// why `ExtensionInputComponent` ignores the placeholder it is handed
    /// (`extension-input.ts:44`).
    pub fn new() -> Input {
        Input {
            value: String::new(),
            cursor: 0,
            prompt: "> ".to_string(),
            placeholder: String::new(),
            placeholder_style: std::sync::Arc::new(ToString::to_string),
            focused: false,
            kill_ring: KillRing::new(),
            last_action: None,
            undo: UndoStack::new(),
            rendered_start_column: 0,
            paste_buffer: String::new(),
            in_paste: false,
        }
    }

    /// `input.ts:46`.
    #[must_use]
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Input {
        self.prompt = prompt.into();
        self
    }

    /// `input.ts:47`.
    #[must_use]
    pub fn with_placeholder(mut self, placeholder: impl Into<String>) -> Input {
        self.placeholder = placeholder.into();
        self
    }

    /// `input.ts:48`.
    #[must_use]
    pub fn with_placeholder_style(
        mut self,
        style: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Input {
        self.placeholder_style = std::sync::Arc::new(style);
        self
    }

    /// `input.ts:54-57`.
    pub fn get_value(&self) -> &str {
        &self.value
    }

    /// `input.ts:58-61`: the cursor clamps to the new value's length.
    pub fn set_value(&mut self, value: &str) {
        self.value = value.to_string();
        self.cursor = self.cursor.min(self.value.chars().count());
    }

    /// pi's key dispatch, in pi's order. `input.ts:73-211`.
    // Long because it mirrors pi's single `handleInput` dispatch 1:1; splitting
    // it would diverge from `input.ts:73-211`.
    #[allow(clippy::too_many_lines)]
    pub fn handle_input(&mut self, data: &str) -> InputEvent {
        let mut data = data.to_string();
        // Bracketed paste: buffer until the end marker arrives, then insert the
        // cleaned text and replay whatever followed it (`input.ts:77-104`).
        if data.contains(PASTE_START) {
            self.in_paste = true;
            self.paste_buffer.clear();
            data = data.replace(PASTE_START, "");
        }
        if self.in_paste {
            self.paste_buffer.push_str(&data);
            let Some(index) = self.paste_buffer.find(PASTE_END) else {
                return InputEvent::None;
            };
            let pasted = self.paste_buffer[..index].to_string();
            let remaining = self.paste_buffer[index + PASTE_END.len()..].to_string();
            self.paste_buffer.clear();
            self.in_paste = false;
            self.handle_paste(&pasted);
            if remaining.is_empty() {
                return InputEvent::None;
            }
            return self.handle_input(&remaining);
        }

        if matches(data.as_str(), action::SELECT_CANCEL) {
            return InputEvent::Cancel;
        }
        if matches(data.as_str(), action::EDITOR_UNDO) {
            self.undo();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::INPUT_SUBMIT) || data == "\n" {
            return InputEvent::Submit(self.value.clone());
        }
        if matches(data.as_str(), action::EDITOR_DELETE_CHAR_BACKWARD) {
            self.handle_backspace();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_DELETE_CHAR_FORWARD) {
            self.handle_forward_delete();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_DELETE_WORD_BACKWARD) {
            self.delete_word_backwards();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_DELETE_WORD_FORWARD) {
            self.delete_word_forward();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_DELETE_TO_LINE_START) {
            self.delete_to_line_start();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_DELETE_TO_LINE_END) {
            self.delete_to_line_end();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_YANK) {
            self.yank();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_YANK_POP) {
            self.yank_pop();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_CURSOR_LEFT) {
            self.last_action = None;
            if self.cursor > 0 {
                self.cursor = grapheme_start_before(&self.value, self.cursor);
            }
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_CURSOR_RIGHT) {
            self.last_action = None;
            if self.cursor < self.value.chars().count() {
                self.cursor = grapheme_start_after(&self.value, self.cursor);
            }
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_CURSOR_LINE_START) {
            self.last_action = None;
            self.cursor = 0;
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_CURSOR_LINE_END) {
            self.last_action = None;
            self.cursor = self.value.chars().count();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_CURSOR_WORD_LEFT) {
            self.move_word_backwards();
            return InputEvent::None;
        }
        if matches(data.as_str(), action::EDITOR_CURSOR_WORD_RIGHT) {
            self.move_word_forwards();
            return InputEvent::None;
        }

        // A Kitty CSI-u printable arrives as a control sequence, so it is
        // decoded before the control-character check (`input.ts:200-205`).
        if let Some(character) = decode_kitty_printable(&data) {
            self.insert_character(&character.to_string());
            return InputEvent::None;
        }
        // Anything printable is inserted, control characters are dropped
        // (`input.ts:207-211`).
        if !has_control_chars(&data) {
            self.insert_character(&data);
        }
        InputEvent::None
    }

    /// The cursor's character index, so a host can place it.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// `input.ts:229-243`: consecutive word characters coalesce into one undo
    /// unit, and a space always starts a new one.
    fn insert_character(&mut self, text: &str) {
        let coalesce =
            !text.chars().any(is_whitespace_char) && self.last_action == Some(LastAction::TypeWord);
        if !coalesce {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        let cursor = self.cursor;
        let mut inserted = String::new();
        inserted.push_str(&chars_before(&self.value, cursor));
        inserted.push_str(text);
        inserted.push_str(&chars_from(&self.value, cursor));
        self.value = inserted;
        self.cursor += text.chars().count();
    }

    /// `input.ts:245-256`: one grapheme back, and one undo unit.
    fn handle_backspace(&mut self) {
        self.last_action = None;
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let start = grapheme_start_before(&self.value, self.cursor);
        self.value = format!(
            "{}{}",
            chars_before(&self.value, start),
            chars_from(&self.value, self.cursor)
        );
        self.cursor = start;
    }

    /// `input.ts:258-268`.
    fn handle_forward_delete(&mut self) {
        self.last_action = None;
        let total = self.value.chars().count();
        if self.cursor >= total {
            return;
        }
        self.push_undo();
        let end = grapheme_start_after(&self.value, self.cursor);
        self.value = format!(
            "{}{}",
            chars_before(&self.value, self.cursor),
            chars_from(&self.value, end)
        );
    }

    /// `input.ts:270-278`: a backward kill, accumulated with the previous one
    /// when the last action was also a kill.
    fn delete_to_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let deleted = chars_before(&self.value, self.cursor);
        self.kill_ring
            .push(&deleted, true, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value = chars_from(&self.value, self.cursor);
        self.cursor = 0;
    }

    /// `input.ts:280-289`.
    fn delete_to_line_end(&mut self) {
        let total = self.value.chars().count();
        if self.cursor >= total {
            return;
        }
        self.push_undo();
        let deleted = chars_from(&self.value, self.cursor);
        self.kill_ring
            .push(&deleted, false, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value = chars_before(&self.value, self.cursor);
    }

    /// `input.ts:291-311`.
    fn delete_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let old_cursor = self.cursor;
        let delete_from = find_word_backward(&self.value, old_cursor, None);
        let deleted = slice_chars(&self.value, delete_from, old_cursor);
        self.kill_ring.push(&deleted, true, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.value = format!(
            "{}{}",
            chars_before(&self.value, delete_from),
            chars_from(&self.value, old_cursor)
        );
        self.cursor = delete_from;
    }

    /// `input.ts:313-330`.
    fn delete_word_forward(&mut self) {
        let total = self.value.chars().count();
        if self.cursor >= total {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let delete_to = find_word_forward(&self.value, self.cursor, None);
        let deleted = slice_chars(&self.value, self.cursor, delete_to);
        self.kill_ring.push(&deleted, false, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.value = format!(
            "{}{}",
            chars_before(&self.value, self.cursor),
            chars_from(&self.value, delete_to)
        );
    }

    /// `input.ts:332-341`.
    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        self.push_undo();
        let cursor = self.cursor;
        self.value = format!(
            "{}{text}{}",
            chars_before(&self.value, cursor),
            chars_from(&self.value, cursor)
        );
        self.cursor += text.chars().count();
        self.last_action = Some(LastAction::Yank);
    }

    /// `input.ts:343-361`: drop what the last yank inserted, rotate the ring,
    /// insert the new entry.
    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();
        let previous = self.kill_ring.peek().unwrap_or_default().to_string();
        self.value = format!(
            "{}{}",
            chars_before(&self.value, self.cursor - previous.chars().count()),
            chars_from(&self.value, self.cursor)
        );
        self.cursor -= previous.chars().count();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or_default().to_string();
        let cursor = self.cursor;
        self.value = format!(
            "{}{text}{}",
            chars_before(&self.value, cursor),
            chars_from(&self.value, cursor)
        );
        self.cursor += text.chars().count();
        self.last_action = Some(LastAction::Yank);
    }

    fn push_undo(&mut self) {
        self.undo.push(InputState {
            value: self.value.clone(),
            cursor: self.cursor,
        });
    }

    /// `input.ts:363-370`.
    fn undo(&mut self) {
        let Some(state) = self.undo.pop() else {
            return;
        };
        self.value = state.value;
        self.cursor = state.cursor;
        self.last_action = None;
    }

    /// `input.ts:372-376`.
    fn move_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_backward(&self.value, self.cursor, None);
    }

    /// `input.ts:378-382`.
    fn move_word_forwards(&mut self) {
        if self.cursor >= self.value.chars().count() {
            return;
        }
        self.last_action = None;
        self.cursor = find_word_forward(&self.value, self.cursor, None);
    }

    /// `input.ts:384-397`: a paste drops its newlines and expands its tabs,
    /// because the value is one line.
    fn handle_paste(&mut self, pasted: &str) {
        self.last_action = None;
        self.push_undo();
        let clean = pasted
            .replace("\r\n", "")
            .replace(['\r', '\n'], "")
            .replace('\t', "    ");
        let cursor = self.cursor;
        self.value = format!(
            "{}{clean}{}",
            chars_before(&self.value, cursor),
            chars_from(&self.value, cursor)
        );
        self.cursor += clean.chars().count();
    }

    /// pi's click handling (`input.ts:229-243` after the paste block): a click
    /// on row 0 moves the cursor to the column under the pointer.
    pub fn click_at(&mut self, column: usize) {
        let visible = std::cmp::max(0, column.saturating_sub(2));
        let target = self.rendered_start_column + visible;
        let mut cursor = self.value.chars().count();
        let mut current = 0usize;
        let mut offset = 0usize;
        for grapheme in self.value.graphemes(true) {
            let next = current + visible_width(grapheme);
            if target < next {
                cursor = offset;
                break;
            }
            current = next;
            offset += grapheme.chars().count();
        }
        self.cursor = cursor;
        self.last_action = None;
    }
}

impl Component for Input {
    /// `input.ts:399-490`: one line, `prompt` then the visible window with a
    /// reverse-video cursor and, when focused, pi's zero-width cursor marker.
    fn render(&self, width: usize) -> Vec<String> {
        let available = width.saturating_sub(visible_width(&self.prompt));
        if available == 0 {
            return vec![crate::ansi::truncate_to_width(
                &self.prompt,
                width,
                "",
                false,
            )];
        }
        if self.value.is_empty() && !self.placeholder.is_empty() {
            let placeholder =
                crate::ansi::truncate_to_width(&self.placeholder, available, "", false);
            let mut graphemes = placeholder.graphemes(true);
            let at_cursor = graphemes.next().unwrap_or(" ");
            let after_cursor = &placeholder[at_cursor.len()..];
            let marker = if self.focused { CURSOR_MARKER } else { "" };
            let cursor_char = format!("\x1b[7m{}\x1b[27m", (self.placeholder_style)(at_cursor));
            let text = format!(
                "{marker}{cursor_char}{}",
                (self.placeholder_style)(after_cursor)
            );
            let padding = " ".repeat(available.saturating_sub(visible_width(&text)));
            return vec![format!("{}{text}{padding}", self.prompt)];
        }

        let total_width = visible_width(&self.value);
        let (visible_text, cursor_display) = if total_width < available {
            (self.value.clone(), self.cursor)
        } else {
            // The cursor's last column is reserved when it sits at the end, so
            // the cursor itself always has a cell (`input.ts:432-449`).
            let scroll_width = if self.cursor == self.value.chars().count() {
                available.saturating_sub(1)
            } else {
                available
            };
            if scroll_width == 0 {
                (String::new(), 0)
            } else {
                let cursor_column = visible_width(&chars_before(&self.value, self.cursor));
                let half = scroll_width / 2;
                let start_column = if cursor_column < half {
                    0
                } else if cursor_column > total_width - half {
                    total_width.saturating_sub(scroll_width)
                } else {
                    cursor_column.saturating_sub(half)
                };
                let visible_text = slice_by_column(&self.value, start_column, scroll_width, true);
                let before = slice_by_column(
                    &self.value,
                    start_column,
                    cursor_column.saturating_sub(start_column),
                    true,
                );
                (visible_text, before.chars().count())
            }
        };

        let marker = if self.focused { CURSOR_MARKER } else { "" };
        let before_cursor = chars_before(&visible_text, cursor_display);
        let rest = chars_from(&visible_text, cursor_display);
        let mut graphemes = rest.graphemes(true);
        let at_cursor = graphemes.next().unwrap_or(" ");
        let after_cursor = chars_from(rest.as_str(), at_cursor.len());
        let text = format!("{before_cursor}{marker}\x1b[7m{at_cursor}\x1b[27m{after_cursor}");
        let padding = " ".repeat(available.saturating_sub(visible_width(&text)));
        vec![format!("{}{text}{padding}", self.prompt)]
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

/// The first `index` characters of `text`.
fn chars_before(text: &str, index: usize) -> String {
    text.chars().take(index).collect()
}

/// Everything from character `index` on.
fn chars_from(text: &str, index: usize) -> String {
    text.chars().skip(index).collect()
}

/// Characters `[start, end)`.
fn slice_chars(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

/// The character index where the grapheme before `cursor` starts.
///
/// `grapheme_indices` yields byte offsets and the cursor counts characters, so
/// the walk is over character lengths rather than offsets.
fn grapheme_start_before(text: &str, cursor: usize) -> usize {
    let prefix: String = text.chars().take(cursor).collect();
    let mut starts = Vec::new();
    let mut index = 0usize;
    for grapheme in prefix.graphemes(true) {
        starts.push(index);
        index += grapheme.chars().count();
    }
    starts.last().copied().unwrap_or(0)
}

/// The character index just past the grapheme at `cursor`.
fn grapheme_start_after(text: &str, cursor: usize) -> usize {
    let rest: String = text.chars().skip(cursor).collect();
    match rest.graphemes(true).next() {
        Some(grapheme) => cursor + grapheme.chars().count(),
        None => cursor,
    }
}

/// `input.ts:207-210`: C0, DEL and C1 all mean "not a character to insert".
fn has_control_chars(data: &str) -> bool {
    data.chars().any(|character| {
        let code = character as u32;
        code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `input.ts:104-211`: cancel, undo and submit are matched before any
    /// editing key, and `\n` submits beside `enter`.
    #[test]
    fn cancel_undo_and_submit_come_first() {
        let mut input = Input::new();
        assert_eq!(input.handle_input("\x1b"), InputEvent::Cancel);
        assert_eq!(input.handle_input("\x03"), InputEvent::Cancel);
        for key in ["abc", "\x1b[D"] {
            input.handle_input(key);
        }
        assert_eq!(
            input.handle_input("\r"),
            InputEvent::Submit("abc".to_string())
        );
        assert_eq!(
            input.handle_input("\n"),
            InputEvent::Submit("abc".to_string())
        );
    }

    /// `ctrl+-` is `tui.editor.undo`; it restores the value and the cursor.
    #[test]
    fn undo_restores_the_value_and_the_cursor() {
        let mut input = Input::new();
        input.handle_input("abc");
        input.handle_input("\x1b[D");
        assert_eq!(input.cursor(), 2);
        // The three letters are one undo unit (`input.ts:229-236`), so undo
        // takes the whole word back and puts the cursor where it started.
        assert_eq!(input.handle_input("\x1f"), InputEvent::None);
        assert_eq!(input.get_value(), "");
        assert_eq!(input.cursor(), 0);
    }

    /// Consecutive word characters are one undo unit; a space starts a new one.
    #[test]
    fn typing_coalesces_into_one_undo_unit_per_word() {
        let mut input = Input::new();
        for key in ["a", "b", "c", " ", "d"] {
            input.handle_input(key);
        }
        assert_eq!(input.get_value(), "abc d");
        input.handle_input("\x1f");
        assert_eq!(input.get_value(), "abc");
        input.handle_input("\x1f");
        assert_eq!(input.get_value(), "");
        input.handle_input("\x1f");
        assert_eq!(input.get_value(), "");
    }

    /// `input.ts:245-268`: deletion is by grapheme, in both directions.
    #[test]
    fn deletion_is_by_grapheme() {
        let mut input = Input::new();
        input.set_value("a👋b");
        // `setValue` clamps the cursor, it does not move it to the end
        // (`input.ts:58-61`), so ctrl+e is the key that gets there.
        input.handle_input("\x05");
        input.handle_input("\x7f");
        assert_eq!(input.get_value(), "a👋");
        input.handle_input("\x1b[H");
        input.handle_input("\x1b[3~");
        assert_eq!(input.get_value(), "👋");
    }

    /// `input.ts:270-289` + `332-361`: consecutive kills accumulate, `ctrl+y`
    /// yanks and `alt+y` yanks the previous entry.
    #[test]
    fn the_kill_ring_accumulates_yanks_and_pops() {
        let mut input = Input::new();
        input.set_value("one two");
        input.handle_input("\x05"); // end
        input.handle_input("\x17"); // ctrl+w
        assert_eq!(input.get_value(), "one ");
        input.handle_input("\x17");
        assert_eq!(input.get_value(), "");
        input.handle_input("\x19"); // ctrl+y
        assert_eq!(input.get_value(), "one two");
        // The two kills accumulated into one entry, so there is nothing for
        // yank-pop to rotate to (`input.ts:344`).
        assert_eq!(input.handle_input("\x1by"), InputEvent::None);
        assert_eq!(input.get_value(), "one two");
    }

    /// `input.ts:272`: `ctrl+u` kills to the start and prepends to the ring.
    #[test]
    fn delete_to_line_start_is_a_backward_kill() {
        let mut input = Input::new();
        input.set_value("cd two");
        input.handle_input("\x05");
        input.handle_input("\x1b[D");
        input.handle_input("\x1b[D");
        input.handle_input("\x1b[D");
        input.handle_input("\x15"); // ctrl+u
        assert_eq!(input.get_value(), "two");
        input.handle_input("\x19");
        assert_eq!(input.get_value(), "cd two");
    }

    /// A Kitty CSI-u printable is decoded before the control-character check
    /// (`input.ts:200-205`).
    #[test]
    fn a_kitty_printable_is_inserted() {
        let mut input = Input::new();
        input.handle_input("\x1b[97u");
        assert_eq!(input.get_value(), "a");
    }

    /// A control character is dropped rather than inserted.
    #[test]
    fn a_control_character_is_dropped() {
        let mut input = Input::new();
        input.handle_input("\x07");
        assert_eq!(input.get_value(), "");
    }

    /// `input.ts:384-397`: a bracketed paste arrives as one chunk, is cleaned of
    /// its newlines, and its tabs become four spaces.
    #[test]
    fn a_bracketed_paste_is_cleaned_and_inserted() {
        let mut input = Input::new();
        input.handle_input("\x1b[200~ab\r\n\tcd\x1b[201~!");
        assert_eq!(input.get_value(), "ab    cd!");
    }

    /// `input.ts:419-428`: an empty value with a placeholder renders the
    /// placeholder with the cursor on its first grapheme.
    #[test]
    fn an_empty_value_renders_the_placeholder() {
        let mut input = Input::new().with_placeholder("http://127.0.0.1");
        let line = input.render(40).join("");
        assert!(line.starts_with("> "), "{line}");
        assert!(line.contains('h'), "the placeholder is drawn: {line}");
        // Focused, the line carries pi's zero-width cursor marker.
        input.set_focused(true);
        assert!(input.render(40)[0].contains(CURSOR_MARKER));
    }

    /// The value is drawn after the prompt, padded to the width.
    #[test]
    fn the_value_is_drawn_after_the_prompt() {
        let mut input = Input::new();
        input.set_value("hello");
        let line = input.render(20).join("");
        let plain: String = strip_ansi(&line);
        assert!(plain.starts_with("> hello"), "{plain}");
        assert_eq!(visible_width(&plain), 20);
    }

    /// A value wider than the line scrolls, keeping the cursor visible.
    #[test]
    fn a_long_value_scrolls_horizontally() {
        let mut input = Input::new();
        input.set_value(&"x".repeat(40));
        let line = input.render(20).join("");
        assert_eq!(visible_width(&strip_ansi(&line)), 20);
    }

    fn strip_ansi(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(character) = chars.next() {
            if character == '\x1b' {
                for next in chars.by_ref() {
                    if next == 'm' || next == '\x07' || next == '~' {
                        break;
                    }
                }
                continue;
            }
            out.push(character);
        }
        out
    }
}
