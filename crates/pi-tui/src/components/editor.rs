//! `Editor`: pi's multi-line text editor.
//!
//! Ground truth: `packages/tui/src/components/editor.ts` (2,472 lines), plus
//! `undo-stack.ts`, `kill-ring.ts`, `word-navigation.ts` and
//! `keybindings.ts:72-158`.
//!
//! What is ported: the state model (lines + cursor), word-aware visual layout
//! (`wordWrapLine`), the render pass (borders, scroll indicators, reverse-video
//! cursor, cursor marker, autocomplete list), the key dispatch order, the
//! fish-style undo coalescing, the emacs kill ring, history, bracketed paste
//! with large-paste markers, jump mode, the sticky column, and the slash-command
//! autocomplete.
//!
//! Deviations, all deliberate:
//!
//! * **Positions are char indices.** pi addresses text in UTF-16 code units;
//!   this port uses char indices end to end, so astral characters move as one
//!   cursor step on both sides.
//! * **Callbacks are polled, not closure-captured.** pi assigns
//!   `onSubmit`/`onChange`; a component that lives behind `Arc<Mutex<..>>` in
//!   the interactive mode cannot capture its host, so the editor records the
//!   last value and the host takes it ([`Editor::take_submitted`],
//!   [`Editor::take_change`]).
//! * **Autocomplete is synchronous.** pi's provider is async and guarded by a
//!   debounce timer and an `AbortController`; this crate owns no timer, so the
//!   provider trait is a plain call ([`AutocompleteProvider`]).
//! * Mouse hit-testing (`handleMouse`, `editor.ts:632-694`) is ported; the
//!   renderer's screen-level text selection over editor rows is not, nor the
//!   external-editor/image paste paths.
//! * **`placeholder`** is an addition: pi's `Editor` has none, and the spec
//!   asks for one. It renders dimmed while the editor is empty and is skipped
//!   by every operation that reads the text.

use std::collections::BTreeMap;
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::{CURSOR_MARKER, visible_width};
use crate::component::{Component, ComponentMutation, Focusable};
use crate::keybindings::{action, matches};
use crate::keys::{decode_printable_key, matches_key};
use crate::kill_ring::KillRing;
use crate::mouse::{MouseButton, MouseEvent, MouseEventKind, MouseEventResult};
use crate::undo_stack::UndoStack;
use crate::word_navigation::{char_slice, find_word_backward, find_word_forward};

type Style = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// `editor.ts:373-382`: the theme's two hooks.
#[derive(Clone)]
pub struct EditorTheme {
    /// Style hook for the top and bottom borders.
    pub border_color: Style,
    /// Theme for the autocomplete popup's `SelectList`.
    pub select_list: crate::components::select_list::SelectListTheme,
}

impl std::fmt::Debug for EditorTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EditorTheme")
    }
}

impl Default for EditorTheme {
    fn default() -> Self {
        EditorTheme {
            border_color: Arc::new(|text: &str| text.to_string()),
            select_list: crate::components::select_list::SelectListTheme::default(),
        }
    }
}

/// `editorOptions`. `editor.ts:242-250`.
#[derive(Debug, Clone, Copy)]
pub struct EditorOptions {
    /// Blank columns on each side of the text.
    pub padding_x: usize,
    /// `editor.ts:250`: clamped to 3..=20.
    pub autocomplete_max_visible: usize,
}

impl Default for EditorOptions {
    fn default() -> Self {
        EditorOptions {
            padding_x: 0,
            autocomplete_max_visible: 5,
        }
    }
}

/// The text state. `editor.ts:218-223`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorState {
    /// The buffer, one entry per logical line.
    pub lines: Vec<String>,
    /// The cursor's line index.
    pub cursor_line: usize,
    /// The cursor's column, in chars.
    pub cursor_col: usize,
}

impl EditorState {
    fn empty() -> EditorState {
        EditorState {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
        }
    }
}

/// The undo unit: text state plus the paste registry. `editor.ts:225-231`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorSnapshot {
    /// The text state the snapshot restores.
    pub state: EditorState,
    /// The paste registry at snapshot time.
    pub pastes: BTreeMap<u64, String>,
    /// The paste id counter at snapshot time.
    pub paste_counter: u64,
}

/// A word-wrap chunk. `editor.ts:106-110`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    /// The chunk's text.
    pub text: String,
    /// The chunk's first char index in its logical line.
    pub start_index: usize,
    /// One past the chunk's last char index.
    pub end_index: usize,
}

/// `utils.ts:54` `cjkBreakRegex`: the scripts that may break between any two
/// characters.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2EFF | 0x3040..=0x30FF | 0x3100..=0x312F | 0x31A0..=0x31BF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
}

/// `editor.ts:37-40`: a segment that is a whole paste marker.
pub fn is_paste_marker(segment: &str) -> bool {
    segment.len() >= 10 && parse_marker(segment).is_some()
}

/// `editor.ts:34`: `PASTE_MARKER_SINGLE`.
fn parse_marker(segment: &str) -> Option<u64> {
    let rest = segment.strip_prefix("[paste #")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let tail = &rest[digits.len()..];
    if tail == "]" || tail.ends_with(" chars]") || tail.ends_with(" lines]") {
        return digits.parse().ok();
    }
    None
}

/// Merge whole paste markers into single segments. `editor.ts:48-103`.
/// Segment positions are char indices.
fn segment_with_markers(text: &str, valid_ids: &BTreeMap<u64, String>) -> Vec<(usize, String)> {
    let base = char_segments(text);
    if valid_ids.is_empty() || !text.contains("[paste #") {
        return base;
    }
    // Byte offset of each char index, so markers can be sliced out of `text`.
    let mut byte_of: Vec<usize> = Vec::with_capacity(base.len() + 1);
    let mut byte = 0usize;
    for (_, grapheme) in &base {
        byte_of.push(byte);
        byte += grapheme.len();
    }
    byte_of.push(byte);

    let mut result: Vec<(usize, String)> = Vec::new();
    let mut char_cursor = 0usize;
    while char_cursor < base.len() {
        let byte_start = byte_of[char_cursor];
        if marker_at(text, byte_start, valid_ids).is_some() {
            let byte_end = marker_end(text, byte_start);
            let mut char_end = char_cursor;
            while char_end < base.len() && byte_of[char_end] < byte_end {
                char_end += 1;
            }
            result.push((char_cursor, text[byte_start..byte_end].to_string()));
            char_cursor = char_end;
            continue;
        }
        result.push(base[char_cursor].clone());
        char_cursor += 1;
    }
    result
}

fn marker_at(text: &str, start: usize, valid_ids: &BTreeMap<u64, String>) -> Option<u64> {
    let rest = &text[start..];
    if !rest.starts_with("[paste #") {
        return None;
    }
    let digits: String = rest["[paste #".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let id: u64 = digits.parse().ok()?;
    if !valid_ids.contains_key(&id) {
        return None;
    }
    // `marker_end` is an offset into `text`; `rest` starts at `start`, so the
    // two have to be reconciled before slicing.
    let end = marker_end(text, start) - start;
    parse_marker(&rest[..end])
}

fn marker_end(text: &str, start: usize) -> usize {
    let rest = &text[start..];
    match rest.find(']') {
        Some(index) => start + index + 1,
        None => start + rest.len(),
    }
}

/// `editor.ts:123-216`: split one logical line into word-wrapped chunks.
/// Positions are char indices.
pub fn word_wrap_line(
    line: &str,
    max_width: usize,
    segments: &[(usize, String)],
) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk {
            text: String::new(),
            start_index: 0,
            end_index: 0,
        }];
    }
    let chars: Vec<char> = line.chars().collect();
    let total_width = visible_width(line);
    if total_width <= max_width {
        return vec![TextChunk {
            text: line.to_string(),
            start_index: 0,
            end_index: chars.len(),
        }];
    }

    let mut chunks: Vec<TextChunk> = Vec::new();
    let mut current_width = 0usize;
    let mut chunk_start = 0usize;
    // Wrap opportunity: after whitespace before a non-whitespace grapheme, or
    // between two CJK characters.
    let mut wrap_opportunity: Option<(usize, usize)> = None;

    for (position, (char_index, grapheme)) in segments.iter().enumerate() {
        let start = *char_index;
        let g_width = visible_width(grapheme);
        let is_ws = !is_paste_marker(grapheme) && grapheme.chars().all(char::is_whitespace);

        if current_width + g_width > max_width {
            match wrap_opportunity {
                Some((opportunity_index, opportunity_width))
                    if current_width - opportunity_width + g_width <= max_width =>
                {
                    chunks.push(TextChunk {
                        text: chars[chunk_start..opportunity_index].iter().collect(),
                        start_index: chunk_start,
                        end_index: opportunity_index,
                    });
                    chunk_start = opportunity_index;
                    current_width -= opportunity_width;
                }
                _ => {
                    if chunk_start < start {
                        chunks.push(TextChunk {
                            text: chars[chunk_start..start].iter().collect(),
                            start_index: chunk_start,
                            end_index: start,
                        });
                        chunk_start = start;
                        current_width = 0;
                    }
                }
            }
            wrap_opportunity = None;
        }

        if g_width > max_width {
            // An atomic segment wider than the width: re-wrap it visually.
            // pi pushes every sub-chunk *except the last*, then carries the
            // last one as the chunk in progress — pushing all of them emits
            // the tail twice, once here and once in the following chunk.
            // `editor.ts:177-184`.
            let sub_chunks = word_wrap_line(grapheme, max_width, &char_segments(grapheme));
            for sub in &sub_chunks[..sub_chunks.len() - 1] {
                chunks.push(TextChunk {
                    text: sub.text.clone(),
                    start_index: start + sub.start_index,
                    end_index: start + sub.end_index,
                });
            }
            let last = sub_chunks
                .last()
                .expect("a re-wrap always produces a chunk");
            chunk_start = start + last.start_index;
            current_width = visible_width(&last.text);
            wrap_opportunity = None;
            continue;
        }

        current_width += g_width;

        let next = segments.get(position + 1);
        if let Some((next_index, next_text)) = next {
            let next_is_ws = next_text.chars().all(char::is_whitespace);
            if is_ws && (is_paste_marker(next_text) || !next_is_ws) {
                wrap_opportunity = Some((*next_index, current_width));
            } else if !is_ws && !next_is_ws {
                let is_cjk_break = (!is_paste_marker(grapheme) && grapheme.chars().all(is_cjk))
                    || (!is_paste_marker(next_text) && next_text.chars().all(is_cjk));
                if is_cjk_break {
                    wrap_opportunity = Some((*next_index, current_width));
                }
            }
        }
    }

    chunks.push(TextChunk {
        text: chars[chunk_start..].iter().collect(),
        start_index: chunk_start,
        end_index: chars.len(),
    });
    chunks
}

/// One segment per grapheme, positioned by char index.
fn char_segments(text: &str) -> Vec<(usize, String)> {
    let mut result = Vec::new();
    for (index, grapheme) in text.graphemes(true).enumerate() {
        result.push((index, grapheme.to_string()));
    }
    result
}

/// What the host gets back from a keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    /// `onSubmit`. `editor.ts:363`.
    Submit(String),
    /// `onChange`. `editor.ts:364`.
    Change(String),
    /// `invalidate` / anything the host must redraw for.
    NeedsRender,
}

/// Autocomplete's answer. `autocomplete.ts`'s `AutocompleteSuggestions`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Suggestions {
    /// The suggestion rows to pick from.
    pub items: Vec<crate::components::select_list::SelectItem>,
    /// The token being completed, replaced on apply.
    pub prefix: String,
}

/// pi's `AutocompleteProvider` (`autocomplete.ts`), synchronous.
pub trait AutocompleteProvider: Send + Sync {
    /// Characters that start a completion token. Default `["@", "#"]`
    /// (`editor.ts:253`).
    fn trigger_characters(&self) -> Vec<char> {
        vec!['@', '#']
    }
    /// Suggestions for the cursor position, or none. `force` is true for
    /// explicit Tab requests (`editor.ts:2388`).
    fn suggest(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<Suggestions>;
    /// Gate on forced (Tab) completion; `autocomplete.ts:847-857` via
    /// `editor.ts:2300-2309`.
    fn should_trigger_file_completion(
        &self,
        _lines: &[String],
        _cursor_line: usize,
        _cursor_col: usize,
    ) -> bool {
        true
    }
    /// Replace the token with `item`. Returns `(lines, cursor_line, cursor_col)`.
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &crate::components::select_list::SelectItem,
        prefix: &str,
    ) -> (Vec<String>, usize, usize);
}

/// A layout line. `editor.ts:231-235`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LayoutLine {
    text: String,
    has_cursor: bool,
    cursor_pos: usize,
}

/// Which action killed text last. `editor.ts:349`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// One visual line's slice of a logical line. `editor.ts:1835`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualLine {
    logical_line: usize,
    start_col: usize,
    length: usize,
}

/// `editor.ts:296-372`.
pub struct Editor {
    state: EditorState,
    focused: bool,
    theme: EditorTheme,
    padding_x: usize,
    last_width: usize,
    last_terminal_rows: usize,
    rendered_visible_line_count: usize,
    rendered_autocomplete_height: usize,
    scroll_offset: usize,
    border_color: Style,
    placeholder: Option<String>,
    provider: Option<Arc<dyn AutocompleteProvider>>,
    autocomplete_list: Option<crate::components::select_list::SelectList>,
    autocomplete_state: Option<AutocompleteState>,
    autocomplete_prefix: String,
    autocomplete_max_visible: usize,
    pastes: BTreeMap<u64, String>,
    paste_counter: u64,
    paste_buffer: String,
    in_paste: bool,
    history: Vec<String>,
    history_index: isize,
    history_draft: Option<EditorState>,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    jump_mode: Option<JumpDirection>,
    preferred_visual_col: Option<usize>,
    snapped_from_cursor_col: Option<usize>,
    undo_stack: UndoStack<EditorSnapshot>,
    /// `editor.ts:365`.
    pub disable_submit: bool,
    /// Host seam for pi's `CustomEditor.renderTopBorder` override
    /// (`components/custom-editor.ts:36-79`): receives `(width,
    /// hidden_line_count)` and returns the full styled border line. When it
    /// returns an empty string the built-in border renders instead.
    pub top_border_decorator: Option<Arc<TopBorderDecorator>>,
    pending: Vec<EditorEvent>,
}

/// A decorator's output depends on live state (spinner frames), so it is a
/// boxed closure rather than a pure styler.
pub type TopBorderDecorator = dyn Fn(usize, usize) -> String + Send + Sync;

/// `editor.ts:332`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutocompleteState {
    /// Opened by typing a trigger; closes when the token stops matching.
    Regular,
    /// Opened by an explicit Tab request.
    Force,
}

/// `editor.ts:353`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpDirection {
    Forward,
    Backward,
}

impl Editor {
    /// `editor.ts:373-382`.
    pub fn new(theme: EditorTheme, options: EditorOptions) -> Editor {
        let padding_x = options.padding_x;
        let max_visible = options.autocomplete_max_visible.clamp(3, 20);
        let border_color = theme.border_color.clone();
        Editor {
            state: EditorState::empty(),
            focused: false,
            theme,
            padding_x,
            last_width: 80,
            last_terminal_rows: 24,
            rendered_visible_line_count: 1,
            rendered_autocomplete_height: 0,
            scroll_offset: 0,
            border_color,
            placeholder: None,
            provider: None,
            autocomplete_list: None,
            autocomplete_state: None,
            autocomplete_prefix: String::new(),
            autocomplete_max_visible: max_visible,
            pastes: BTreeMap::new(),
            paste_counter: 0,
            paste_buffer: String::new(),
            in_paste: false,
            history: Vec::new(),
            history_index: -1,
            history_draft: None,
            kill_ring: KillRing::new(),
            last_action: None,
            jump_mode: None,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            undo_stack: UndoStack::new(),
            disable_submit: false,
            top_border_decorator: None,
            pending: Vec::new(),
        }
    }

    /// pi's `editor.ts:417-425`.
    pub fn set_autocomplete_provider(&mut self, provider: Option<Arc<dyn AutocompleteProvider>>) {
        self.cancel_autocomplete();
        self.provider = provider;
    }

    /// The spec's placeholder; pi's editor has none.
    pub fn set_placeholder(&mut self, placeholder: Option<String>) {
        self.placeholder = placeholder;
    }

    /// Swap the border style hook; the chat mode repaints it on thinking
    /// level and bash-mode changes (`interactive-mode.ts:4700-4707`).
    pub fn set_border_color(&mut self, color: Style) {
        self.border_color = color;
    }

    /// The current border style hook.
    pub fn border_color(&self) -> &Style {
        &self.border_color
    }

    /// The horizontal padding the editor renders with.
    pub fn padding_x(&self) -> usize {
        self.padding_x
    }

    /// `editor.ts:397-403`.
    pub fn set_padding_x(&mut self, padding: usize) {
        if self.padding_x != padding {
            self.padding_x = padding;
            self.pending.push(EditorEvent::NeedsRender);
        }
    }

    /// The clamped autocomplete row cap.
    pub fn autocomplete_max_visible(&self) -> usize {
        self.autocomplete_max_visible
    }

    /// `editor.ts:409-415`.
    pub fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        let clamped = max_visible.clamp(3, 20);
        if self.autocomplete_max_visible != clamped {
            self.autocomplete_max_visible = clamped;
            self.pending.push(EditorEvent::NeedsRender);
        }
    }

    /// `editor.ts:427-437`.
    pub fn add_to_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.first().map(String::as_str) == Some(trimmed) {
            return;
        }
        self.history.insert(0, trimmed.to_string());
        // pi caps history at 100 entries.
        if self.history.len() > 100 {
            self.history.pop();
        }
    }

    /// The prompt history, newest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Take the events recorded since the last call.
    pub fn take_events(&mut self) -> Vec<EditorEvent> {
        std::mem::take(&mut self.pending)
    }

    /// The last submitted value, if any.
    pub fn take_submitted(&mut self) -> Option<String> {
        let mut found = None;
        for event in self.take_events() {
            if let EditorEvent::Submit(text) = event {
                found = Some(text);
            }
        }
        found
    }

    /// The last change, if any.
    pub fn take_change(&mut self) -> Option<String> {
        let mut found = None;
        for event in self.take_events() {
            if let EditorEvent::Change(text) = event {
                found = Some(text);
            }
        }
        found
    }

    /// `editor.ts:1085`.
    pub fn get_text(&self) -> String {
        self.state.lines.join("\n")
    }

    /// `editor.ts:1089-1101`: paste markers expanded back to their content.
    pub fn get_expanded_text(&self) -> String {
        self.expand_paste_markers(&self.get_text())
    }

    fn expand_paste_markers(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (id, content) in &self.pastes {
            result = result.replace(&format!("[paste #{id}]"), content);
            // The marker may carry the `+N lines` / `N chars` suffix.
            if let Some((_, tail)) = result.split_once(&format!("[paste #{id} "))
                && let Some(end) = tail.find(']')
            {
                let full = format!("[paste #{id} {}", &tail[..=end]);
                result = result.replace(&full, content);
            }
        }
        result
    }

    /// `editor.ts:1106`.
    pub fn get_lines(&self) -> &[String] {
        &self.state.lines
    }

    /// `editor.ts:1110`.
    pub fn get_cursor(&self) -> (usize, usize) {
        (self.state.cursor_line, self.state.cursor_col)
    }

    /// Move the cursor to `(line, col)`, clamped to the buffer. pi does not
    /// expose this; a host restoring a session or driving scripted input needs
    /// it, and it is the seam the tests use instead of synthetic key runs.
    pub fn set_cursor(&mut self, line: usize, col: usize) {
        let line = line.min(self.state.lines.len().saturating_sub(1));
        self.state.cursor_line = line;
        let length = self.line_len(line);
        self.set_cursor_col(col.min(length));
    }

    /// `editor.ts:1114-1131`.
    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.exit_history_browsing();
        let normalized = Self::normalize_text(text);
        if self.get_text() != normalized {
            self.push_undo_snapshot();
        }
        self.pastes.clear();
        self.paste_counter = 0;
        self.set_text_internal(&normalized, CursorPlacement::End);
    }

    /// `editor.ts:1133-1145`: atomic insert, one undo unit.
    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cancel_autocomplete();
        self.push_undo_snapshot();
        self.last_action = None;
        self.exit_history_browsing();
        self.insert_text_at_cursor_internal(text);
    }

    /// `editor.ts:1147-1150`.
    fn normalize_text(text: &str) -> String {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
    }

    fn emit_change(&mut self) {
        let text = self.get_text();
        self.pending.push(EditorEvent::Change(text));
    }

    /// `editor.ts:493-504`.
    fn set_text_internal(&mut self, text: &str, placement: CursorPlacement) {
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        self.state.lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        self.state.cursor_line = match placement {
            CursorPlacement::Start => 0,
            CursorPlacement::End => self.state.lines.len().saturating_sub(1),
        };
        let col = match placement {
            CursorPlacement::Start => 0,
            CursorPlacement::End => self
                .state
                .lines
                .get(self.state.cursor_line)
                .map_or(0, |line| line.chars().count()),
        };
        self.set_cursor_col(col);
        self.scroll_offset = 0;
        self.emit_change();
    }

    fn line(&self, index: usize) -> &str {
        self.state.lines.get(index).map_or("", String::as_str)
    }

    fn line_len(&self, index: usize) -> usize {
        self.line(index).chars().count()
    }

    fn before_cursor(&self) -> String {
        char_slice(self.line(self.state.cursor_line), 0, self.state.cursor_col)
            .unwrap_or("")
            .to_string()
    }

    fn segments(&self, text: &str) -> Vec<(usize, String)> {
        segment_with_markers(text, &self.pastes)
    }

    /// `editor.ts:1468-1471`.
    fn set_cursor_col(&mut self, col: usize) {
        self.state.cursor_col = col;
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    // ------------------------------------------------------------- layout

    /// `editor.ts:997-1083`.
    fn layout_text(&self, content_width: usize) -> Vec<LayoutLine> {
        let mut layout_lines = Vec::new();
        if self.state.lines.len() == 1 && self.state.lines[0].is_empty() {
            layout_lines.push(LayoutLine {
                text: String::new(),
                has_cursor: true,
                cursor_pos: 0,
            });
            return layout_lines;
        }
        for (index, line) in self.state.lines.iter().enumerate() {
            let is_current = index == self.state.cursor_line;
            if visible_width(line) <= content_width {
                layout_lines.push(LayoutLine {
                    text: line.clone(),
                    has_cursor: is_current,
                    cursor_pos: if is_current { self.state.cursor_col } else { 0 },
                });
                continue;
            }
            let segments = self.segments(line);
            let chunks = word_wrap_line(line, content_width, &segments);
            let chunk_count = chunks.len();
            for (position, chunk) in chunks.into_iter().enumerate() {
                let is_last = position + 1 == chunk_count;
                let (has_cursor, cursor_pos) = if !is_current {
                    (false, 0)
                } else if is_last {
                    (
                        self.state.cursor_col >= chunk.start_index,
                        self.state.cursor_col - chunk.start_index,
                    )
                } else if self.state.cursor_col >= chunk.start_index
                    && self.state.cursor_col < chunk.end_index
                {
                    let adjusted =
                        (self.state.cursor_col - chunk.start_index).min(chunk.text.chars().count());
                    (true, adjusted)
                } else {
                    (false, 0)
                };
                layout_lines.push(LayoutLine {
                    text: chunk.text,
                    has_cursor,
                    cursor_pos,
                });
            }
        }
        layout_lines
    }

    /// `editor.ts:1835-1863`.
    fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let mut visual_lines = Vec::new();
        for (index, line) in self.state.lines.iter().enumerate() {
            if line.is_empty() {
                visual_lines.push(VisualLine {
                    logical_line: index,
                    start_col: 0,
                    length: 0,
                });
            } else if visible_width(line) <= width {
                visual_lines.push(VisualLine {
                    logical_line: index,
                    start_col: 0,
                    length: line.chars().count(),
                });
            } else {
                let segments = self.segments(line);
                for chunk in word_wrap_line(line, width, &segments) {
                    visual_lines.push(VisualLine {
                        logical_line: index,
                        start_col: chunk.start_index,
                        length: chunk.end_index - chunk.start_index,
                    });
                }
            }
        }
        visual_lines
    }

    /// `editor.ts:1865-1885`.
    fn find_visual_line_at(visual_lines: &[VisualLine], line: usize, col: usize) -> usize {
        for (index, vl) in visual_lines.iter().enumerate() {
            if vl.logical_line != line {
                continue;
            }
            let offset = col as isize - vl.start_col as isize;
            let is_last_segment = visual_lines
                .get(index + 1)
                .is_none_or(|next| next.logical_line != vl.logical_line);
            if offset >= 0
                && (offset < vl.length as isize
                    || (is_last_segment && offset == vl.length as isize))
            {
                return index;
            }
        }
        visual_lines.len().saturating_sub(1)
    }

    fn find_current_visual_line(&self, visual_lines: &[VisualLine]) -> usize {
        Self::find_visual_line_at(visual_lines, self.state.cursor_line, self.state.cursor_col)
    }

    fn is_on_first_visual_line(&self) -> bool {
        let visual_lines = self.build_visual_line_map(self.last_width);
        self.find_current_visual_line(&visual_lines) == 0
    }

    fn is_on_last_visual_line(&self) -> bool {
        let visual_lines = self.build_visual_line_map(self.last_width);
        self.find_current_visual_line(&visual_lines) + 1 == visual_lines.len()
    }

    fn is_editor_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }

    /// `editor.ts:1580-1611`: the sticky-column decision table.
    fn compute_vertical_move_column(
        &mut self,
        current_visual_col: usize,
        source_max_visual_col: usize,
        target_max_visual_col: usize,
    ) -> usize {
        let has_preferred = self.preferred_visual_col.is_some();
        let cursor_in_middle = current_visual_col < source_max_visual_col;
        let target_too_short = target_max_visual_col < current_visual_col;
        if !has_preferred || cursor_in_middle {
            if target_too_short {
                self.preferred_visual_col = Some(current_visual_col);
                return target_max_visual_col;
            }
            self.preferred_visual_col = None;
            return current_visual_col;
        }
        let preferred = self.preferred_visual_col.unwrap_or(0);
        if target_too_short || target_max_visual_col < preferred {
            return target_max_visual_col;
        }
        self.preferred_visual_col = None;
        preferred
    }

    /// `editor.ts:1478-1578`.
    fn move_to_visual_line(
        &mut self,
        visual_lines: &[VisualLine],
        current_visual_line: usize,
        target_visual_line: usize,
    ) {
        let Some(current_vl) = visual_lines.get(current_visual_line).copied() else {
            return;
        };
        let Some(target_vl) = visual_lines.get(target_visual_line).copied() else {
            return;
        };
        let current_visual_col = match self.snapped_from_cursor_col {
            Some(snapped) => {
                let vl_index =
                    Self::find_visual_line_at(visual_lines, current_vl.logical_line, snapped);
                snapped - visual_lines[vl_index].start_col
            }
            None => self.state.cursor_col - current_vl.start_col,
        };
        let is_last_source_segment = visual_lines
            .get(current_visual_line + 1)
            .is_none_or(|next| next.logical_line != current_vl.logical_line);
        let source_max = if is_last_source_segment {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1)
        };
        let is_last_target_segment = visual_lines
            .get(target_visual_line + 1)
            .is_none_or(|next| next.logical_line != target_vl.logical_line);
        let target_max = if is_last_target_segment {
            target_vl.length
        } else {
            target_vl.length.saturating_sub(1)
        };

        let move_to = self.compute_vertical_move_column(current_visual_col, source_max, target_max);
        self.state.cursor_line = target_vl.logical_line;
        let target_col = target_vl.start_col + move_to;
        let logical_length = self.line_len(target_vl.logical_line);
        self.state.cursor_col = target_col.min(logical_length);

        // Snap out of a multi-grapheme (paste-marker) segment.
        let line = self.line(self.state.cursor_line).to_string();
        for (index, segment) in self.segments(&line) {
            let length = segment.chars().count();
            if index > self.state.cursor_col {
                break;
            }
            if length <= 1 {
                continue;
            }
            if self.state.cursor_col < index + length {
                let is_continuation = index < target_vl.start_col;
                let is_moving_down = target_visual_line > current_visual_line;
                if is_continuation && is_moving_down {
                    let segment_end = index + length;
                    let mut next = target_visual_line + 1;
                    while visual_lines.get(next).is_some_and(|vl| {
                        vl.logical_line == target_vl.logical_line && vl.start_col < segment_end
                    }) {
                        next += 1;
                    }
                    if next < visual_lines.len() {
                        self.move_to_visual_line(visual_lines, current_visual_line, next);
                        return;
                    }
                }
                self.snapped_from_cursor_col = Some(self.state.cursor_col);
                self.state.cursor_col = index;
                return;
            }
        }
        self.snapped_from_cursor_col = None;
    }

    /// `editor.ts:1893-1958`.
    fn move_cursor(&mut self, delta_line: isize, delta_col: isize) {
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current_visual_line = self.find_current_visual_line(&visual_lines);
        if delta_line != 0 {
            let target = current_visual_line as isize + delta_line;
            if target >= 0 && (target as usize) < visual_lines.len() {
                self.move_to_visual_line(&visual_lines, current_visual_line, target as usize);
            }
        }
        if delta_col != 0 {
            let current_len = self.line_len(self.state.cursor_line);
            if delta_col > 0 {
                if self.state.cursor_col < current_len {
                    let rest = char_slice(
                        self.line(self.state.cursor_line),
                        self.state.cursor_col,
                        current_len,
                    )
                    .unwrap_or("")
                    .to_string();
                    let step = self
                        .segments(&rest)
                        .first()
                        .map_or(1, |(_, grapheme)| grapheme.chars().count());
                    let next = self.state.cursor_col + step;
                    self.set_cursor_col(next.min(current_len));
                } else if self.state.cursor_line + 1 < self.state.lines.len() {
                    self.state.cursor_line += 1;
                    self.set_cursor_col(0);
                } else if let Some(vl) = visual_lines.get(current_visual_line).copied() {
                    self.preferred_visual_col = Some(self.state.cursor_col - vl.start_col);
                }
            } else {
                if self.state.cursor_col > 0 {
                    let before =
                        char_slice(self.line(self.state.cursor_line), 0, self.state.cursor_col)
                            .unwrap_or("")
                            .to_string();
                    let step = self
                        .segments(&before)
                        .last()
                        .map_or(1, |(_, grapheme)| grapheme.chars().count());
                    let next = self.state.cursor_col.saturating_sub(step);
                    self.set_cursor_col(next);
                } else if self.state.cursor_line > 0 {
                    self.state.cursor_line -= 1;
                    let length = self.line_len(self.state.cursor_line);
                    self.set_cursor_col(length);
                }
            }
        }
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
        }
    }

    /// `editor.ts:1960-1970`.
    fn page_scroll(&mut self, direction: isize, page_size: usize) {
        self.last_action = None;
        let visual_lines = self.build_visual_line_map(self.last_width);
        let current = self.find_current_visual_line(&visual_lines);
        let target = (current as isize + direction * page_size as isize)
            .clamp(0, visual_lines.len().saturating_sub(1) as isize) as usize;
        self.move_to_visual_line(&visual_lines, current, target);
    }

    // ------------------------------------------------------------ movement

    /// `editor.ts:1613-1616`.
    fn move_to_line_start(&mut self) {
        self.last_action = None;
        self.set_cursor_col(0);
    }

    /// `editor.ts:1618-1622`.
    fn move_to_line_end(&mut self) {
        self.last_action = None;
        let length = self.line_len(self.state.cursor_line);
        self.set_cursor_col(length);
    }

    fn is_paste_marker_segment(text: &str) -> bool {
        is_paste_marker(text)
    }

    /// `editor.ts:1972-1995`.
    fn move_word_backwards(&mut self) {
        self.last_action = None;
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let length = self.line_len(self.state.cursor_line);
                self.set_cursor_col(length);
            }
            return;
        }
        let line = self.line(self.state.cursor_line).to_string();
        let atomic = Self::is_paste_marker_segment;
        let position = find_word_backward(&line, self.state.cursor_col, Some(&atomic));
        self.set_cursor_col(position.min(self.line_len(self.state.cursor_line)));
    }

    /// `editor.ts:2167-2187`.
    fn move_word_forwards(&mut self) {
        self.last_action = None;
        let length = self.line_len(self.state.cursor_line);
        if self.state.cursor_col >= length {
            if self.state.cursor_line + 1 < self.state.lines.len() {
                self.state.cursor_line += 1;
                self.set_cursor_col(0);
            }
            return;
        }
        let line = self.line(self.state.cursor_line).to_string();
        let atomic = Self::is_paste_marker_segment;
        let position = find_word_forward(&line, self.state.cursor_col, Some(&atomic));
        self.set_cursor_col(position.min(length));
    }

    // ------------------------------------------------------------- editing

    /// `editor.ts:1156-1198`.
    fn insert_text_at_cursor_internal(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = Self::normalize_text(text);
        let inserted: Vec<String> = normalized.split('\n').map(str::to_string).collect();
        let line_index = self.state.cursor_line;
        let cursor = self.state.cursor_col;
        let current = self.line(line_index).to_string();
        let before = char_slice(&current, 0, cursor).unwrap_or("").to_string();
        let after = char_slice(&current, cursor, current.chars().count())
            .unwrap_or("")
            .to_string();
        if inserted.len() == 1 {
            let mut next = before.clone();
            next.push_str(&inserted[0]);
            next.push_str(&after);
            self.state.lines[line_index] = next;
            let new_col = cursor + inserted[0].chars().count();
            self.set_cursor_col(new_col);
        } else {
            let mut lines: Vec<String> =
                Vec::with_capacity(self.state.lines.len() + inserted.len());
            lines.extend(self.state.lines[..line_index].iter().cloned());
            let mut first = before;
            first.push_str(&inserted[0]);
            lines.push(first);
            lines.extend(inserted[1..inserted.len() - 1].iter().cloned());
            let mut last = inserted[inserted.len() - 1].clone();
            last.push_str(&after);
            lines.push(last);
            lines.extend(self.state.lines[line_index + 1..].iter().cloned());
            self.state.lines = lines;
            self.state.cursor_line = line_index + inserted.len() - 1;
            self.set_cursor_col(inserted[inserted.len() - 1].chars().count());
        }
        self.emit_change();
    }

    /// `editor.ts:1200-1257`.
    fn insert_character(&mut self, ch: char) {
        self.exit_history_browsing();
        // Fish-style undo coalescing.
        if ch.is_whitespace() || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo_snapshot();
        }
        self.last_action = Some(LastAction::TypeWord);
        let line_index = self.state.cursor_line;
        let cursor = self.state.cursor_col;
        let current = self.line(line_index).to_string();
        let before = char_slice(&current, 0, cursor).unwrap_or("").to_string();
        let after = char_slice(&current, cursor, current.chars().count())
            .unwrap_or("")
            .to_string();
        let mut next = before;
        next.push(ch);
        next.push_str(&after);
        self.state.lines[line_index] = next;
        let new_col = cursor + 1;
        self.set_cursor_col(new_col);
        self.emit_change();
        self.autocomplete_after_edit(ch);
    }

    /// `editor.ts:1230-1257` plus `updateAutocomplete` on an open picker.
    fn autocomplete_after_edit(&mut self, ch: char) {
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
            return;
        }
        let text_before = self.before_cursor();
        let trigger = (ch == '/' && self.is_at_start_of_message())
            || (self.trigger_characters().contains(&ch) && self.matches_trigger(&text_before));
        let continuation =
            (ch.is_alphanumeric() || ch == '.' || ch == '-' || ch == '_' || is_cjk(ch))
                && (self.is_in_slash_command_context(&text_before)
                    || self.matches_trigger(&text_before));
        if trigger || continuation {
            self.try_trigger_autocomplete();
        }
    }

    fn trigger_characters(&self) -> Vec<char> {
        // pi keeps the defaults and adds the provider's, minus `/` and
        // whitespace (`editor.ts:2340-2350`): the slash is not a token trigger
        // here because it has its own rule at the start of a message
        // (`editor.ts:1232-1234`), which `autocomplete_after_edit` applies
        // separately. Replacing the default instead would take `@` away from
        // every provider that does not declare it.
        let mut triggers: Vec<char> = Self::default_trigger_characters();
        if let Some(provider) = &self.provider {
            for character in provider.trigger_characters() {
                if character == '/' || character.is_whitespace() || triggers.contains(&character) {
                    continue;
                }
                triggers.push(character);
            }
        }
        triggers
    }

    fn default_trigger_characters() -> Vec<char> {
        vec!['@', '#']
    }

    /// pi's trigger/debounce pattern, reduced to its decision: the text before
    /// the cursor ends with a trigger character (or a quoted `@` token) at a
    /// token boundary. `editor.ts:255-276`.
    fn matches_trigger(&self, text_before: &str) -> bool {
        let triggers = self.trigger_characters();
        let Some(last) = text_before.chars().last() else {
            return false;
        };
        if !triggers.contains(&last) {
            return false;
        }
        // The trigger must start a token: start of line or after whitespace /
        // one of the bracket characters pi allows to open prose tokens.
        let preceding: Vec<char> = text_before.chars().collect();
        preceding
            .get(preceding.len().wrapping_sub(2))
            .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '<' | '`' | '"'))
    }

    /// `editor.ts:1327-1350`.
    fn add_new_line(&mut self) {
        self.last_action = None;
        self.push_undo_snapshot();
        let line_index = self.state.cursor_line;
        let cursor = self.state.cursor_col;
        let current = self.line(line_index).to_string();
        let before = char_slice(&current, 0, cursor).unwrap_or("").to_string();
        let after = char_slice(&current, cursor, current.chars().count())
            .unwrap_or("")
            .to_string();
        self.state.lines[line_index] = before;
        self.state.lines.insert(line_index + 1, after);
        self.state.cursor_line += 1;
        self.set_cursor_col(0);
        self.emit_change();
    }

    /// `editor.ts:1363-1377`.
    fn submit_value(&mut self) {
        self.cancel_autocomplete();
        let result = self
            .expand_paste_markers(&self.get_text())
            .trim()
            .to_string();
        self.state = EditorState::empty();
        self.pastes.clear();
        self.paste_counter = 0;
        self.exit_history_browsing();
        self.scroll_offset = 0;
        self.undo_stack.clear();
        self.last_action = None;
        self.pending.push(EditorEvent::Change(String::new()));
        self.pending.push(EditorEvent::Submit(result));
    }

    /// `editor.ts:1379-1465`.
    fn handle_backspace(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;
        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();
            let line_index = self.state.cursor_line;
            let cursor = self.state.cursor_col;
            let before = char_slice(self.line(line_index), 0, cursor)
                .unwrap_or("")
                .to_string();
            let segments = self.segments(&before);
            let (marker, grapheme) = segments.last().cloned().unwrap_or((0, "a".to_string()));
            let length = grapheme.chars().count();
            // A paste marker is one atomic unit; dropping it drops its registry
            // entry and renumbers the rest.
            if is_paste_marker(&grapheme) && let Some(id) = parse_marker(&grapheme) {
                self.pastes.remove(&id);
                self.paste_counter = self.paste_counter.saturating_sub(1);
                let higher: Vec<u64> = self
                    .pastes
                    .keys()
                    .copied()
                    .filter(|key| *key > id)
                    .collect();
                for key in higher {
                    if let Some(content) = self.pastes.remove(&key) {
                        self.pastes.insert(key - 1, content);
                    }
                }
                let renumbered = self.state.lines.clone();
                self.state.lines = renumbered
                    .iter()
                    .map(|line| renumber_paste_markers(line, id))
                    .collect();
            }
            let current = self.line(line_index).to_string();
            let before_part = char_slice(&current, 0, cursor.saturating_sub(length))
                .unwrap_or("")
                .to_string();
            let after = char_slice(&current, cursor, current.chars().count())
                .unwrap_or("")
                .to_string();
            self.state.lines[line_index] = format!("{before_part}{after}");
            let _ = marker;
            let next = self.state.cursor_col - length;
            self.set_cursor_col(next);
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();
            let current = self.line(self.state.cursor_line).to_string();
            let previous = self.line(self.state.cursor_line - 1).to_string();
            self.state.lines[self.state.cursor_line - 1] = format!("{previous}{current}");
            self.state.lines.remove(self.state.cursor_line);
            self.state.cursor_line -= 1;
            let length = previous.chars().count();
            self.set_cursor_col(length);
        }
        self.emit_change();
        self.autocomplete_after_deletion();
    }

    fn autocomplete_after_deletion(&mut self) {
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
            return;
        }
        let text_before = self.before_cursor();
        if self.is_in_slash_command_context(&text_before) || self.matches_trigger(&text_before) {
            self.try_trigger_autocomplete();
        }
    }

    /// `editor.ts:1778-1833`.
    fn handle_forward_delete(&mut self) {
        self.exit_history_browsing();
        self.last_action = None;
        let line_index = self.state.cursor_line;
        let cursor = self.state.cursor_col;
        let current = self.line(line_index).to_string();
        let length = current.chars().count();
        if cursor < length {
            self.push_undo_snapshot();
            let rest = char_slice(&current, cursor, length)
                .unwrap_or("")
                .to_string();
            let step = self
                .segments(&rest)
                .first()
                .map_or(1, |(_, grapheme)| grapheme.chars().count());
            let before = char_slice(&current, 0, cursor).unwrap_or("").to_string();
            let after = char_slice(&current, (cursor + step).min(length), length)
                .unwrap_or("")
                .to_string();
            self.state.lines[line_index] = format!("{before}{after}");
        } else if line_index + 1 < self.state.lines.len() {
            self.push_undo_snapshot();
            let next_line = self.line(line_index + 1).to_string();
            self.state.lines[line_index] = format!("{current}{next_line}");
            self.state.lines.remove(line_index + 1);
        }
        self.emit_change();
        self.autocomplete_after_deletion();
    }

    fn kill(&mut self, text: &str, prepend: bool) {
        self.kill_ring
            .push(text, prepend, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
    }

    /// `editor.ts:1624-1657`.
    fn delete_to_start_of_line(&mut self) {
        self.exit_history_browsing();
        let line_index = self.state.cursor_line;
        let cursor = self.state.cursor_col;
        let current = self.line(line_index).to_string();
        if cursor > 0 {
            self.push_undo_snapshot();
            let deleted = char_slice(&current, 0, cursor).unwrap_or("").to_string();
            self.kill(&deleted, true);
            let rest = char_slice(&current, cursor, current.chars().count())
                .unwrap_or("")
                .to_string();
            self.state.lines[line_index] = rest;
            self.set_cursor_col(0);
        } else if line_index > 0 {
            self.push_undo_snapshot();
            self.kill("\n", true);
            let previous = self.line(line_index - 1).to_string();
            self.state.lines[line_index - 1] = format!("{previous}{current}");
            self.state.lines.remove(line_index);
            self.state.cursor_line -= 1;
            let length = previous.chars().count();
            self.set_cursor_col(length);
        }
        self.emit_change();
    }

    /// `editor.ts:1659-1689`.
    fn delete_to_end_of_line(&mut self) {
        self.exit_history_browsing();
        let line_index = self.state.cursor_line;
        let cursor = self.state.cursor_col;
        let current = self.line(line_index).to_string();
        let length = current.chars().count();
        if cursor < length {
            self.push_undo_snapshot();
            let deleted = char_slice(&current, cursor, length)
                .unwrap_or("")
                .to_string();
            self.kill(&deleted, false);
            self.state.lines[line_index] =
                char_slice(&current, 0, cursor).unwrap_or("").to_string();
        } else if line_index + 1 < self.state.lines.len() {
            self.push_undo_snapshot();
            self.kill("\n", false);
            let next_line = self.line(line_index + 1).to_string();
            self.state.lines[line_index] = format!("{current}{next_line}");
            self.state.lines.remove(line_index + 1);
        }
        self.emit_change();
    }

    /// `editor.ts:1691-1734`.
    fn delete_word_backwards(&mut self) {
        self.exit_history_browsing();
        let line_index = self.state.cursor_line;
        let cursor = self.state.cursor_col;
        let current = self.line(line_index).to_string();
        if cursor == 0 {
            if line_index > 0 {
                self.push_undo_snapshot();
                self.kill("\n", true);
                let previous = self.line(line_index - 1).to_string();
                self.state.lines[line_index - 1] = format!("{previous}{current}");
                self.state.lines.remove(line_index);
                self.state.cursor_line -= 1;
                let length = previous.chars().count();
                self.set_cursor_col(length);
            }
            self.emit_change();
            return;
        }
        self.push_undo_snapshot();
        let was_kill = self.last_action == Some(LastAction::Kill);
        let old_cursor = cursor;
        self.move_word_backwards();
        let delete_from = self.state.cursor_col;
        let deleted = char_slice(&current, delete_from, old_cursor)
            .unwrap_or("")
            .to_string();
        self.kill_ring.push(&deleted, true, was_kill);
        self.last_action = Some(LastAction::Kill);
        let before = char_slice(&current, 0, delete_from)
            .unwrap_or("")
            .to_string();
        let rest = char_slice(&current, old_cursor, current.chars().count())
            .unwrap_or("")
            .to_string();
        self.state.lines[line_index] = format!("{before}{rest}");
        self.set_cursor_col(delete_from);
        self.emit_change();
    }

    /// `editor.ts:1736-1776`.
    fn delete_word_forward(&mut self) {
        self.exit_history_browsing();
        let line_index = self.state.cursor_line;
        let cursor = self.state.cursor_col;
        let current = self.line(line_index).to_string();
        let length = current.chars().count();
        if cursor >= length {
            if line_index + 1 < self.state.lines.len() {
                self.push_undo_snapshot();
                self.kill("\n", false);
                let next_line = self.line(line_index + 1).to_string();
                self.state.lines[line_index] = format!("{current}{next_line}");
                self.state.lines.remove(line_index + 1);
            }
            self.emit_change();
            return;
        }
        self.push_undo_snapshot();
        let was_kill = self.last_action == Some(LastAction::Kill);
        let old_cursor = cursor;
        self.move_word_forwards();
        let delete_to = self.state.cursor_col;
        self.set_cursor_col(old_cursor);
        let deleted = char_slice(&current, old_cursor, delete_to)
            .unwrap_or("")
            .to_string();
        self.kill_ring.push(&deleted, false, was_kill);
        self.last_action = Some(LastAction::Kill);
        let before = char_slice(&current, 0, old_cursor)
            .unwrap_or("")
            .to_string();
        let after = char_slice(&current, delete_to, length)
            .unwrap_or("")
            .to_string();
        self.state.lines[line_index] = format!("{before}{after}");
        self.emit_change();
    }

    /// `editor.ts:2115-2134`.
    fn push_undo_snapshot(&mut self) {
        self.undo_stack.push(EditorSnapshot {
            state: self.state.clone(),
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        });
    }

    fn undo(&mut self) {
        self.exit_history_browsing();
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.state = snapshot.state;
        self.pastes = snapshot.pastes;
        self.paste_counter = snapshot.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
        self.emit_change();
    }

    fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    /// `editor.ts:1997-2032`.
    fn yank(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }
        self.push_undo_snapshot();
        let text = self.kill_ring.peek().unwrap_or("").to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    /// `editor.ts:2012-2032`.
    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo_snapshot();
        self.delete_yanked_text();
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or("").to_string();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    /// `editor.ts:2034-2075`.
    fn insert_yanked_text(&mut self, text: &str) {
        self.exit_history_browsing();
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        let line_index = self.state.cursor_line;
        let cursor = self.state.cursor_col;
        let current = self.line(line_index).to_string();
        let before = char_slice(&current, 0, cursor).unwrap_or("").to_string();
        let after = char_slice(&current, cursor, current.chars().count())
            .unwrap_or("")
            .to_string();
        if lines.len() == 1 {
            self.state.lines[line_index] = format!("{before}{text}{after}");
            self.set_cursor_col(cursor + text.chars().count());
        } else {
            let mut merged_first = before;
            merged_first.push_str(&lines[0]);
            self.state.lines[line_index] = merged_first;
            for (offset, extra) in lines[1..lines.len() - 1].iter().enumerate() {
                self.state
                    .lines
                    .insert(line_index + 1 + offset, extra.clone());
            }
            let last_index = line_index + lines.len() - 1;
            let mut merged_last = lines[lines.len() - 1].clone();
            merged_last.push_str(&after);
            self.state.lines.insert(last_index, merged_last);
            self.state.cursor_line = last_index;
            self.set_cursor_col(lines[lines.len() - 1].chars().count());
        }
        self.emit_change();
    }

    /// `editor.ts:2077-2113`.
    fn delete_yanked_text(&mut self) {
        let Some(yanked) = self.kill_ring.peek().map(str::to_string) else {
            return;
        };
        let yank_lines: Vec<String> = yanked.split('\n').map(str::to_string).collect();
        if yank_lines.len() == 1 {
            let line_index = self.state.cursor_line;
            let cursor = self.state.cursor_col;
            let current = self.line(line_index).to_string();
            let delete_len = yanked.chars().count();
            let before = char_slice(&current, 0, cursor.saturating_sub(delete_len))
                .unwrap_or("")
                .to_string();
            let after = char_slice(&current, cursor, current.chars().count())
                .unwrap_or("")
                .to_string();
            self.state.lines[line_index] = format!("{before}{after}");
            let next = self.state.cursor_col - delete_len;
            self.set_cursor_col(next);
        } else {
            let start_line = self.state.cursor_line - (yank_lines.len() - 1);
            let start_col = self.line_len(start_line) - yank_lines[0].chars().count();
            let after_cursor = char_slice(
                self.line(self.state.cursor_line),
                self.state.cursor_col,
                self.line_len(self.state.cursor_line),
            )
            .unwrap_or("")
            .to_string();
            let before_yank = char_slice(self.line(start_line), 0, start_col)
                .unwrap_or("")
                .to_string();
            self.state.lines.splice(
                start_line..start_line + yank_lines.len(),
                [format!("{before_yank}{after_cursor}")],
            );
            self.state.cursor_line = start_line;
            self.set_cursor_col(start_col);
        }
        self.emit_change();
    }

    /// `editor.ts:455-491`.
    fn navigate_history(&mut self, direction: isize) {
        self.last_action = None;
        if self.history.is_empty() {
            return;
        }
        let new_index = self.history_index - direction;
        if !(-1..=self.history.len() as isize - 1).contains(&new_index) {
            return;
        }
        if self.history_index == -1 && new_index >= 0 {
            self.push_undo_snapshot();
            self.history_draft = Some(self.state.clone());
        }
        self.history_index = new_index;
        if self.history_index == -1 {
            let draft = self.history_draft.take();
            self.preferred_visual_col = None;
            self.snapped_from_cursor_col = None;
            self.scroll_offset = 0;
            match draft {
                Some(draft) => self.state = draft,
                None => self.set_text_internal("", CursorPlacement::End),
            }
            self.emit_change();
        } else {
            let entry = self.history[self.history_index as usize].clone();
            let placement = if direction == -1 {
                CursorPlacement::Start
            } else {
                CursorPlacement::End
            };
            self.set_text_internal(&entry, placement);
        }
    }

    fn exit_history_browsing(&mut self) {
        self.history_index = -1;
        self.history_draft = None;
    }

    /// The history-browsing index; `-1` is the live draft.
    pub fn history_index(&self) -> isize {
        self.history_index
    }

    /// `editor.ts:1259-1325`.
    fn handle_paste(&mut self, pasted: &str) {
        self.cancel_autocomplete();
        self.exit_history_browsing();
        self.last_action = None;
        self.push_undo_snapshot();
        let decoded = decode_csi_u_control(pasted);
        let clean = Self::normalize_text(&decoded);
        let filtered: String = clean
            .chars()
            .filter(|c| *c == '\n' || *c != '\u{7f}' && !c.is_control())
            .collect();
        let mut filtered = filtered;
        // A path-like paste after a word character gets a leading space.
        if filtered.starts_with(['/', '~', '.']) {
            let line = self.line(self.state.cursor_line).to_string();
            let before = char_slice(&line, 0, self.state.cursor_col)
                .unwrap_or("")
                .to_string();
            if let Some(last) = before.chars().last() && (last.is_alphanumeric() || last == '_') {
                filtered.insert(0, ' ');
            }
        }
        let pasted_lines = filtered.split('\n').count();
        let total_chars = filtered.chars().count();
        if pasted_lines > 10 || total_chars > 1000 {
            self.paste_counter += 1;
            let id = self.paste_counter;
            self.pastes.insert(id, filtered.clone());
            let marker = if pasted_lines > 10 {
                format!("[paste #{id} +{pasted_lines} lines]")
            } else {
                format!("[paste #{id} {total_chars} chars]")
            };
            self.insert_text_at_cursor_internal(&marker);
            return;
        }
        self.insert_text_at_cursor_internal(&filtered);
    }

    /// `editor.ts:2137-2165`.
    fn jump_to_char(&mut self, needle: char, direction: JumpDirection) {
        self.last_action = None;
        let forward = direction == JumpDirection::Forward;
        let start_line = self.state.cursor_line;
        let count = self.state.lines.len();
        let indices: Vec<usize> = if forward {
            (start_line..count).collect()
        } else {
            (0..=start_line).rev().collect()
        };
        for line_index in indices {
            let line = self.line(line_index).to_string();
            let chars: Vec<char> = line.chars().collect();
            let search_from: isize = if line_index == start_line {
                if forward {
                    self.state.cursor_col as isize + 1
                } else {
                    self.state.cursor_col as isize - 1
                }
            } else {
                if forward { 0 } else { chars.len() as isize }
            };
            // JS `lastIndexOf` clamps a negative fromIndex to 0, so a backward
            // jump on the first character still searches the whole line.
            let from = (search_from.max(0) as usize).min(chars.len());
            let found = if forward {
                chars[from..].iter().position(|c| *c == needle)
            } else {
                chars[..from].iter().rposition(|c| *c == needle)
            };
            if let Some(offset) = found {
                self.state.cursor_line = line_index;
                let position = if forward { from + offset } else { offset };
                self.set_cursor_col(position);
                return;
            }
        }
    }

    // -------------------------------------------------------- autocomplete

    /// `editor.ts:2189-2199`.
    fn is_slash_menu_allowed(&self) -> bool {
        self.state.cursor_line == 0
    }

    fn is_at_start_of_message(&self) -> bool {
        if !self.is_slash_menu_allowed() {
            return false;
        }
        let trimmed = self.before_cursor().trim().to_string();
        trimmed.is_empty() || trimmed == "/"
    }

    fn is_in_slash_command_context(&self, text_before: &str) -> bool {
        self.is_slash_menu_allowed() && text_before.trim_start().starts_with('/')
    }

    /// `editor.ts:2217-2234`.
    fn best_match_index(
        items: &[crate::components::select_list::SelectItem],
        prefix: &str,
    ) -> Option<usize> {
        if prefix.is_empty() {
            return None;
        }
        let mut first_prefix = None;
        for (index, item) in items.iter().enumerate() {
            if item.value == prefix {
                return Some(index);
            }
            if first_prefix.is_none() && item.value.starts_with(prefix) {
                first_prefix = Some(index);
            }
        }
        first_prefix
    }

    fn try_trigger_autocomplete(&mut self) {
        self.request_autocomplete(false);
    }

    fn update_autocomplete(&mut self) {
        let force = self.autocomplete_state == Some(AutocompleteState::Force);
        self.request_autocomplete(force);
    }

    /// pi's `requestAutocomplete` (`editor.ts:2297-2316`), minus the debounce
    /// and the abort token: the provider is synchronous here.
    fn request_autocomplete(&mut self, force: bool) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        if force
            && !provider.should_trigger_file_completion(
                &self.state.lines,
                self.state.cursor_line,
                self.state.cursor_col,
            )
        {
            return;
        }
        let suggestions = provider.suggest(
            &self.state.lines,
            self.state.cursor_line,
            self.state.cursor_col,
            force,
        );
        match suggestions {
            Some(suggestions) if !suggestions.items.is_empty() => {
                let layout = if suggestions.prefix.starts_with('/') {
                    crate::components::select_list::SelectListLayout {
                        min_primary_column_width: Some(12),
                        max_primary_column_width: Some(32),
                    }
                } else {
                    crate::components::select_list::SelectListLayout::default()
                };
                let mut list = crate::components::select_list::SelectList::new(
                    suggestions.items.clone(),
                    self.autocomplete_max_visible,
                    self.theme.select_list.clone(),
                    layout,
                );
                if let Some(index) = Self::best_match_index(&suggestions.items, &suggestions.prefix)
                {
                    list.set_selected_index(index);
                }
                self.autocomplete_prefix = suggestions.prefix;
                self.autocomplete_list = Some(list);
                self.autocomplete_state = Some(if force {
                    AutocompleteState::Force
                } else {
                    AutocompleteState::Regular
                });
            }
            _ => self.clear_autocomplete_ui(),
        }
    }

    fn clear_autocomplete_ui(&mut self) {
        self.autocomplete_state = None;
        self.autocomplete_list = None;
        self.autocomplete_prefix.clear();
    }

    /// `editor.ts:2459-2462`.
    pub fn cancel_autocomplete(&mut self) {
        self.clear_autocomplete_ui();
    }

    /// `editor.ts:2464-2466`.
    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete_state.is_some()
    }

    fn apply_autocomplete_selection(&mut self) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let Some(list) = &self.autocomplete_list else {
            return;
        };
        let Some(selected) = list.selected_item().cloned() else {
            return;
        };
        self.push_undo_snapshot();
        self.last_action = None;
        let (lines, cursor_line, cursor_col) = provider.apply_completion(
            &self.state.lines,
            self.state.cursor_line,
            self.state.cursor_col,
            &selected,
            &self.autocomplete_prefix,
        );
        self.state.lines = lines;
        self.state.cursor_line = cursor_line;
        self.set_cursor_col(cursor_col);
        self.cancel_autocomplete();
        self.emit_change();
    }

    // ------------------------------------------------------------- render

    /// `editor.ts:278-293`.
    fn scroll_border(direction: &str, hidden: usize, width: usize) -> String {
        let label = format!(" {direction} {hidden} more ");
        let label_width = visible_width(&label);
        if label_width + 2 <= width {
            let left = (width - label_width) / 2;
            let left_borders = "─".repeat(left);
            let right_borders = "─".repeat(width - left - label_width);
            return format!("{left_borders}{label}{right_borders}");
        }
        let indicator = format!("─── {direction} {hidden} more ");
        let remaining = width as isize - visible_width(&indicator) as isize;
        if remaining >= 0 {
            return format!("{indicator}{}", "─".repeat(remaining as usize));
        }
        let ellipsis = "...".chars().take(width).collect::<String>();
        let indicator_width = width.saturating_sub(visible_width(&ellipsis));
        let head = crate::ansi::slice_by_column(&indicator, 0, indicator_width, true);
        format!("{head}{ellipsis}")
    }

    /// `editor.ts:510-518`.
    fn render_top_border(&self, width: usize, hidden: usize) -> String {
        if let Some(decorator) = &self.top_border_decorator {
            let decorated = decorator(width, hidden);
            if !decorated.is_empty() {
                return decorated;
            }
        }
        let border = if hidden > 0 {
            Self::scroll_border("↑", hidden, width)
        } else {
            "─".repeat(width)
        };
        (self.border_color)(&border)
    }

    fn render_bottom_border(&self, width: usize, hidden: usize) -> String {
        let border = if hidden > 0 {
            Self::scroll_border("↓", hidden, width)
        } else {
            "─".repeat(width)
        };
        (self.border_color)(&border)
    }

    /// `editor.ts:520-629`.
    pub fn render_lines(&mut self, width: usize, terminal_rows: usize) -> Vec<String> {
        let max_padding = width.saturating_sub(1) / 2;
        let padding_x = self.padding_x.min(max_padding);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let layout_width = content_width
            .saturating_sub(usize::from(padding_x == 0))
            .max(1);
        self.last_width = layout_width;
        let layout_lines = self.layout_text(layout_width);
        let max_visible_lines = self.visible_window();
        let _ = terminal_rows;

        let cursor_line_index = layout_lines
            .iter()
            .position(|line| line.has_cursor)
            .unwrap_or(0);
        if cursor_line_index < self.scroll_offset {
            self.scroll_offset = cursor_line_index;
        } else if cursor_line_index >= self.scroll_offset + max_visible_lines {
            self.scroll_offset = cursor_line_index + 1 - max_visible_lines;
        }
        let max_scroll_offset = layout_lines.len().saturating_sub(max_visible_lines);
        self.scroll_offset = self.scroll_offset.min(max_scroll_offset);

        let visible: Vec<LayoutLine> = layout_lines
            .iter()
            .skip(self.scroll_offset)
            .take(max_visible_lines)
            .cloned()
            .collect();
        self.rendered_visible_line_count = visible.len();

        let mut result: Vec<String> = Vec::new();
        result.push(self.render_top_border(width, self.scroll_offset));
        let left_padding = " ".repeat(padding_x);
        let right_padding = left_padding.clone();
        let emit_cursor_marker = self.focused;
        for layout_line in &visible {
            let mut display = layout_line.text.clone();
            let mut line_width = visible_width(&layout_line.text);
            let mut cursor_in_padding = false;
            if layout_line.has_cursor {
                let chars: Vec<char> = display.chars().collect();
                let position = layout_line.cursor_pos.min(chars.len());
                let before: String = chars[..position].iter().collect();
                let after: Vec<char> = chars[position..].to_vec();
                let marker = if emit_cursor_marker {
                    CURSOR_MARKER
                } else {
                    ""
                };
                if after.is_empty() {
                    let cursor = "\x1b[7m \x1b[0m";
                    display = format!("{before}{marker}{cursor}");
                    line_width += 1;
                    if line_width > content_width && padding_x > 0 {
                        cursor_in_padding = true;
                    }
                } else {
                    let first: String = after.iter().take(1).collect();
                    let rest: String = after[1..].iter().collect();
                    let cursor = format!("\x1b[7m{first}\x1b[0m");
                    display = format!("{before}{marker}{cursor}{rest}");
                }
            }
            let padding = " ".repeat(content_width.saturating_sub(line_width));
            let line_right = if cursor_in_padding && !right_padding.is_empty() {
                &right_padding[1..]
            } else {
                right_padding.as_str()
            };
            result.push(format!("{left_padding}{display}{padding}{line_right}"));
        }
        let lines_below = layout_lines.len() - (self.scroll_offset + visible.len());
        result.push(self.render_bottom_border(width, lines_below));

        self.rendered_autocomplete_height = 0;
        if self.autocomplete_state.is_some() && let Some(list) = &self.autocomplete_list {
            let autocomplete_lines = list.render(content_width);
            self.rendered_autocomplete_height = autocomplete_lines.len();
            for line in autocomplete_lines {
                let line_padding = " ".repeat(content_width.saturating_sub(visible_width(&line)));
                result.push(format!("{left_padding}{line}{line_padding}{right_padding}"));
            }
        }
        result
    }

    // -------------------------------------------------------------- input

    /// `editor.ts:695-996`, in pi's dispatch order.
    // Long because it mirrors pi's single `handleInput` dispatch 1:1; splitting
    // it would obscure the upstream ordering it documents.
    #[allow(clippy::too_many_lines)]
    pub fn handle_input(&mut self, data: &str) {
        // 1. Character jump mode.
        if self.jump_mode.is_some() {
            if matches(data, action::EDITOR_JUMP_FORWARD)
                || matches(data, action::EDITOR_JUMP_BACKWARD)
            {
                self.jump_mode = None;
                return;
            }
            if let Some(printable) = printable_of(data) {
                let direction = self.jump_mode.take().unwrap_or(JumpDirection::Forward);
                self.jump_to_char(printable, direction);
                return;
            }
            self.jump_mode = None;
        }

        // 2. Bracketed paste.
        let data = if data.contains("\x1b[200~") {
            self.in_paste = true;
            self.paste_buffer.clear();
            data.replacen("\x1b[200~", "", 1)
        } else {
            data.to_string()
        };
        let data = data.as_str();
        if self.in_paste {
            self.paste_buffer.push_str(data);
            if let Some(end) = self.paste_buffer.find("\x1b[201~") {
                let content = self.paste_buffer[..end].to_string();
                if !content.is_empty() {
                    self.handle_paste(&content);
                }
                self.in_paste = false;
                let remaining = self.paste_buffer[end + 6..].to_string();
                self.paste_buffer.clear();
                if !remaining.is_empty() {
                    self.handle_input(&remaining);
                }
                return;
            }
            return;
        }

        // 3. Ctrl+C is the parent's (a copy with a selection, an interrupt).
        if matches(data, action::INPUT_COPY) {
            return;
        }

        // 4. Undo.
        if matches(data, action::EDITOR_UNDO) {
            self.undo();
            return;
        }

        // 5. An open autocomplete owns the next keys.
        if self.autocomplete_state.is_some() && self.autocomplete_list.is_some() {
            if matches(data, action::SELECT_CANCEL) {
                self.cancel_autocomplete();
                return;
            }
            if matches(data, action::SELECT_UP) || matches(data, action::SELECT_DOWN) {
                if let Some(list) = &mut self.autocomplete_list {
                    list.handle_input(data);
                }
                return;
            }
            if matches(data, action::INPUT_TAB) {
                self.apply_autocomplete_selection();
                return;
            }
            if matches(data, action::SELECT_CONFIRM) {
                let submit_after = self.autocomplete_prefix.starts_with('/');
                self.apply_autocomplete_selection();
                if submit_after {
                    if !self.disable_submit {
                        self.submit_value();
                    }
                    return;
                }
                return;
            }
        }

        // 6. Tab triggers completion.
        if matches(data, action::INPUT_TAB) && self.autocomplete_state.is_none() {
            self.handle_tab_completion();
            return;
        }

        // 7. Deletions.
        if matches(data, action::EDITOR_DELETE_TO_LINE_END) {
            self.delete_to_end_of_line();
            return;
        }
        if matches(data, action::EDITOR_DELETE_TO_LINE_START) {
            self.delete_to_start_of_line();
            return;
        }
        if matches(data, action::EDITOR_DELETE_WORD_BACKWARD) {
            self.delete_word_backwards();
            return;
        }
        if matches(data, action::EDITOR_DELETE_WORD_FORWARD) {
            self.delete_word_forward();
            return;
        }
        if matches(data, action::EDITOR_DELETE_CHAR_BACKWARD)
            || matches_key(data, "shift+backspace")
        {
            self.handle_backspace();
            return;
        }
        if matches(data, action::EDITOR_DELETE_CHAR_FORWARD) || matches_key(data, "shift+delete") {
            self.handle_forward_delete();
            return;
        }

        // 8. Kill ring.
        if matches(data, action::EDITOR_YANK) {
            self.yank();
            return;
        }
        if matches(data, action::EDITOR_YANK_POP) {
            self.yank_pop();
            return;
        }

        // 9. Explicit history actions.
        if matches(data, action::EDITOR_HISTORY_PREVIOUS) {
            self.cancel_autocomplete();
            self.navigate_history(-1);
            return;
        }
        if matches(data, action::EDITOR_HISTORY_NEXT) {
            self.cancel_autocomplete();
            self.navigate_history(1);
            return;
        }

        // 10. Movement.
        if matches(data, action::EDITOR_CURSOR_LINE_START) {
            self.move_to_line_start();
            return;
        }
        if matches(data, action::EDITOR_CURSOR_LINE_END) {
            self.move_to_line_end();
            return;
        }
        if matches(data, action::EDITOR_CURSOR_WORD_LEFT) {
            self.move_word_backwards();
            return;
        }
        if matches(data, action::EDITOR_CURSOR_WORD_RIGHT) {
            self.move_word_forwards();
            return;
        }

        // 11. Newline.
        if is_newline_key(data) {
            if self.should_submit_on_backslash_enter(data) {
                self.handle_backspace();
                self.submit_value();
                return;
            }
            self.add_new_line();
            return;
        }

        // 12. Submit.
        if matches(data, action::INPUT_SUBMIT) {
            if self.disable_submit {
                return;
            }
            // Terminals without shift+enter: `\` before the cursor means newline.
            let before = self.before_cursor();
            if let Some(last) = before.chars().last() && last == '\\' {
                self.handle_backspace();
                self.add_new_line();
                return;
            }
            self.submit_value();
            return;
        }

        // 13. Arrows.
        if matches(data, action::EDITOR_CURSOR_UP) {
            if self.is_on_first_visual_line()
                && (self.is_editor_empty() || self.history_index > -1 || self.state.cursor_col == 0)
            {
                self.navigate_history(-1);
            } else if self.is_on_first_visual_line() {
                self.move_to_line_start();
            } else {
                self.move_cursor(-1, 0);
            }
            return;
        }
        if matches(data, action::EDITOR_CURSOR_DOWN) {
            if self.history_index > -1 && self.is_on_last_visual_line() {
                self.navigate_history(1);
            } else if self.is_on_last_visual_line() {
                self.move_to_line_end();
            } else {
                self.move_cursor(1, 0);
            }
            return;
        }
        if matches(data, action::EDITOR_CURSOR_RIGHT) {
            self.move_cursor(0, 1);
            return;
        }
        if matches(data, action::EDITOR_CURSOR_LEFT) {
            self.move_cursor(0, -1);
            return;
        }

        // 14. Page keys.
        if matches(data, action::EDITOR_PAGE_UP) {
            let page = self.visible_window();
            self.page_scroll(-1, page);
            return;
        }
        if matches(data, action::EDITOR_PAGE_DOWN) {
            let page = self.visible_window();
            self.page_scroll(1, page);
            return;
        }

        // 15. Jump hotkeys.
        if matches(data, action::EDITOR_JUMP_FORWARD) {
            self.jump_mode = Some(JumpDirection::Forward);
            return;
        }
        if matches(data, action::EDITOR_JUMP_BACKWARD) {
            self.jump_mode = Some(JumpDirection::Backward);
            return;
        }

        if matches_key(data, "shift+space") {
            self.insert_character(' ');
            return;
        }

        if let Some(printable) = printable_of(data) {
            self.insert_character(printable);
            return;
        }
        if let Some(first) = data.chars().next() && first >= ' ' && data.chars().count() == 1 {
            self.insert_character(first);
        }
    }

    /// pi's visible-line and page size: `max(5, floor(rows * 0.3))`
    /// (`editor.ts:538`, `:1961`). pi reads `tui.terminal.rows` live; this port
    /// keeps the height the host last rendered at.
    fn visible_window(&self) -> usize {
        ((self.last_terminal_rows as f64 * 0.3).floor() as usize).max(5)
    }

    /// The terminal height the host renders at.
    pub fn set_terminal_rows(&mut self, rows: usize) {
        self.last_terminal_rows = rows;
    }

    /// The terminal height the host last reported.
    pub fn terminal_rows(&self) -> usize {
        self.last_terminal_rows
    }

    /// `editor.ts:1352-1361`: with shift+enter available, a `\` before the
    /// cursor submits instead of inserting a newline.
    fn should_submit_on_backslash_enter(&self, data: &str) -> bool {
        if self.disable_submit || !matches_key(data, "enter") {
            return false;
        }
        let has_shift_enter = crate::keybindings::keys_for(action::INPUT_SUBMIT)
            .iter()
            .any(|key| *key == "shift+enter" || *key == "shift+return");
        if !has_shift_enter {
            return false;
        }
        let before = self.before_cursor();
        before.ends_with('\\')
    }

    /// How many lines the last render used for the editor body.
    pub fn rendered_visible_line_count(&self) -> usize {
        self.rendered_visible_line_count
    }

    /// The lines the last render spent on the autocomplete list.
    pub fn rendered_autocomplete_height(&self) -> usize {
        self.rendered_autocomplete_height
    }

    /// How many undo snapshots are stacked.
    pub fn undo_stack_depth(&self) -> usize {
        self.undo_depth()
    }

    /// The kill ring, for hosts that inspect it.
    pub fn kill_ring(&self) -> &KillRing {
        &self.kill_ring
    }

    /// The configured placeholder, if any.
    pub fn placeholder(&self) -> Option<&str> {
        self.placeholder.as_deref()
    }

    /// Whether the editor is empty for the host's `ctrl+d` rule
    /// (`custom-editor.ts:117-124`).
    pub fn is_empty(&self) -> bool {
        self.get_text().is_empty()
    }

    /// `editor.ts:2665-2282`: tab completes the slash command when the cursor is
    /// in one, and forces a provider completion otherwise.
    fn handle_tab_completion(&mut self) {
        if self.provider.is_none() {
            return;
        }
        let before = self.before_cursor();
        if self.is_in_slash_command_context(&before) && !before.trim_start().contains(' ') {
            self.request_autocomplete(false);
        } else {
            self.request_autocomplete(true);
        }
    }
}

/// Where a programmatic `set_text` puts the cursor. `editor.ts:493`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorPlacement {
    Start,
    End,
}

/// `editor.ts:851-873`: every spelling of "insert a newline" pi accepts.
fn is_newline_key(data: &str) -> bool {
    if matches(data, action::INPUT_NEW_LINE) {
        return true;
    }
    let first = data.chars().next().unwrap_or('\0');
    (first == '\n' && data.len() > 1)
        || data == "\x1b\r"
        || data == "\x1b[13;2~"
        || (data.len() > 1 && data.contains('\x1b') && data.contains('\r'))
        || data == "\n"
}

/// `editor.ts:1000-1027`: printable, including Kitty CSI-u printable keys.
fn printable_of(data: &str) -> Option<char> {
    if let Some(c) = decode_printable_key(data) {
        return Some(c);
    }
    let first = data.chars().next()?;
    if data.chars().count() == 1 && first >= ' ' {
        return Some(first);
    }
    None
}

/// tmux popups re-encode control bytes inside a bracketed paste as CSI-u
/// sequences. `editor.ts:1268-1276`.
fn decode_csi_u_control(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("\x1b[") {
        let tail = &rest[index + 2..];
        if let Some(end) = tail.find('u') {
            let body = &tail[..end];
            let mut parts = body.split(';');
            if let (Some(code), Some("5")) = (parts.next(), parts.next())
                && let Ok(codepoint) = code.parse::<u32>()
            {
                let decoded = match codepoint {
                    97..=122 => Some((codepoint - 96) as u8 as char),
                    65..=90 => Some((codepoint - 64) as u8 as char),
                    _ => None,
                };
                if let Some(decoded) = decoded {
                    result.push_str(&rest[..index]);
                    result.push(decoded);
                    rest = &tail[end + 1..];
                    continue;
                }
            }
        }
        result.push_str(&rest[..index + 2]);
        // Not a decodable control: drop the introducer and keep scanning, so a
        // pasted CSI-u sequence still reaches the ordinary escape handling.
        rest = tail;
    }
    result.push_str(rest);
    result
}

/// Renumber every marker above `removed_id`. `editor.ts:1416-1423`.
fn renumber_paste_markers(line: &str, removed_id: u64) -> String {
    let mut result = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(index) = rest.find("[paste #") {
        result.push_str(&rest[..index]);
        let tail = &rest[index..];
        let digits: String = tail["[paste #".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let Some(id) = digits.parse::<u64>().ok() else {
            result.push_str(&tail[..1]);
            rest = &tail[1..];
            continue;
        };
        let Some(end) = tail.find(']') else {
            result.push_str(tail);
            rest = "";
            continue;
        };
        let full = &tail[..=end];
        if id <= removed_id {
            result.push_str(full);
        } else {
            let suffix = &full["[paste #".len() + digits.len()..];
            {
                use std::fmt::Write as _;
                let _ = write!(result, "[paste #{}{suffix}", id - 1);
            }
        }
        rest = &tail[end + 1..];
    }
    result.push_str(rest);
    result
}

impl Focusable for Editor {
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl Component for Editor {
    fn render(&self, width: usize) -> Vec<String> {
        // `render_lines` needs `&mut self` (scroll state); the component seam
        // cannot give it, so a `&self` render lays out without scrolling.
        let max_padding = width.saturating_sub(1) / 2;
        let padding_x = self.padding_x.min(max_padding);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let layout_width = content_width
            .saturating_sub(usize::from(padding_x == 0))
            .max(1);
        let layout_lines = self.layout_text(layout_width);
        let visible: Vec<LayoutLine> = layout_lines
            .iter()
            .skip(self.scroll_offset)
            .take(self.visible_window())
            .cloned()
            .collect();
        let mut result = vec![self.render_top_border(width, self.scroll_offset)];
        let left_padding = " ".repeat(padding_x);
        for layout_line in &visible {
            let mut display = layout_line.text.clone();
            if layout_line.has_cursor {
                let chars: Vec<char> = display.chars().collect();
                let position = layout_line.cursor_pos.min(chars.len());
                let before: String = chars[..position].iter().collect();
                let after: Vec<char> = chars[position..].to_vec();
                if after.is_empty() {
                    display = format!("{before}\x1b[7m \x1b[0m");
                } else {
                    let first: String = after.iter().take(1).collect();
                    let rest: String = after[1..].iter().collect();
                    display = format!(
                        "{before}{}\x1b[7m{first}\x1b[0m{rest}",
                        if self.focused { CURSOR_MARKER } else { "" }
                    );
                }
            }
            let padding = " ".repeat(content_width.saturating_sub(visible_width(&display)));
            result.push(format!("{left_padding}{display}{padding}{left_padding}"));
        }
        let lines_below = layout_lines
            .len()
            .saturating_sub(self.scroll_offset + visible.len());
        result.push(self.render_bottom_border(width, lines_below));
        if let Some(list) = &self.autocomplete_list {
            for line in list.render(content_width) {
                let line_padding = " ".repeat(content_width.saturating_sub(visible_width(&line)));
                result.push(format!("{left_padding}{line}{line_padding}{left_padding}"));
            }
        }
        result
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        Editor::handle_input(self, data);
        Some(ComponentMutation { consume: true })
    }

    /// `Editor.handleMouse`, `editor.ts:632-694`.
    ///
    /// Only two gestures are claimed: a click inside the autocomplete popup,
    /// and a click on the editor body, which positions the cursor. Press,
    /// drag and release stay unhandled on purpose so the renderer's
    /// screen-level text selection can run over the editor rows.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        let autocomplete_start_row = self.rendered_visible_line_count as i64 + 2;
        if self.autocomplete_state.is_some() && self.autocomplete_list.is_some() {
            let height = self.rendered_autocomplete_height;
            if event.y >= autocomplete_start_row && event.y < autocomplete_start_row + height as i64
            {
                let max_padding = (event.width as i64 - 1).max(0) / 2;
                let padding_x = (self.padding_x as i64).min(max_padding);
                let content_width = (event.width as i64 - padding_x * 2).max(1) as usize;
                let forwarded = MouseEvent {
                    x: event.x - padding_x,
                    y: event.y - autocomplete_start_row,
                    width: content_width,
                    height,
                    ..*event
                };
                let result = self
                    .autocomplete_list
                    .as_mut()
                    .and_then(|list| list.handle_mouse(&forwarded));
                return result.map(|result| MouseEventResult {
                    focus: Some(true),
                    ..result
                });
            }
        }

        if event.kind != MouseEventKind::Click || event.button != MouseButton::Left {
            return None;
        }
        // Row 0 is the editor's top border; rows above or past the body are
        // still ours, they just do not move the cursor.
        if event.y <= 0 || event.y > self.rendered_visible_line_count as i64 {
            return Some(MouseEventResult::focus());
        }

        let visual_lines = self.build_visual_line_map(self.last_width);
        let visual_line_index = self.scroll_offset + event.y as usize - 1;
        let Some(visual_line) = visual_lines.get(visual_line_index).copied() else {
            return Some(MouseEventResult::focus());
        };
        let logical_line: Vec<char> = self
            .state
            .lines
            .get(visual_line.logical_line)
            .map(|line| line.chars().collect())
            .unwrap_or_default();
        let chunk: String = logical_line
            .iter()
            .skip(visual_line.start_col)
            .take(visual_line.length)
            .collect();

        let max_padding = (event.width as i64 - 1).max(0) / 2;
        let padding_x = (self.padding_x as i64).min(max_padding);
        let target_column = (event.x - padding_x).max(0) as usize;
        let mut visible_column = 0usize;
        let mut target_index = chunk.len();
        let mut last_grapheme_index = 0usize;
        for (index, grapheme) in chunk.grapheme_indices(true) {
            let next_column = visible_column + crate::ansi::visible_width(grapheme);
            last_grapheme_index = index;
            if target_column < next_column {
                target_index = index;
                break;
            }
            visible_column = next_column;
        }
        let is_last_segment = visual_line_index + 1 >= visual_lines.len()
            || visual_lines[visual_line_index + 1].logical_line != visual_line.logical_line;
        if !is_last_segment && target_index == chunk.len() && !chunk.is_empty() {
            target_index = last_grapheme_index;
        }

        // `grapheme.index` is a byte offset into `chunk` and the cursor counts
        // chars, so slice with the offset directly: no char/byte conversion.
        self.state.cursor_line = visual_line.logical_line;
        // `setCursorCol` takes the value unchanged (`editor.ts:1468-1472`).
        self.set_cursor_col(visual_line.start_col + chunk[..target_index].chars().count());
        self.last_action = None;
        self.exit_history_browsing();
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
        }
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
    use crate::components::select_list::SelectItem;

    fn editor() -> Editor {
        Editor::new(EditorTheme::default(), EditorOptions::default())
    }

    /// pi's key ids as the sequences a host actually delivers.
    const UP: &str = "\x1b[A";
    const DOWN: &str = "\x1b[B";
    const LEFT: &str = "\x1b[D";
    const RIGHT: &str = "\x1b[C";
    const HOME: &str = "\x1b[H";
    const END: &str = "\x1b[F";
    const BACKSPACE: &str = "\x7f";
    const CTRL_W: &str = "\x17";
    const CTRL_U: &str = "\x15";
    const ALT_D: &str = "\x1bd";
    const CTRL_Y: &str = "\x19";
    const ALT_Y: &str = "\x1by";
    const CTRL_UNDERSCORE: &str = "\x1f";
    const CTRL_BRACKET_RIGHT: &str = "\x1d";
    const ENTER: &str = "\r";
    const ALT_LEFT: &str = "\x1b[1;3D";
    const ALT_RIGHT: &str = "\x1b[1;3C";

    fn type_text(editor: &mut Editor, text: &str) {
        for ch in text.chars() {
            editor.handle_input(&ch.to_string());
        }
        editor.take_events();
    }

    fn plain(editor: &mut Editor, width: usize) -> Vec<String> {
        editor
            .render_lines(width, 24)
            .into_iter()
            .map(|line| {
                crate::ansi::strip_terminal_sequences(&line)
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn body(editor: &mut Editor, width: usize) -> Vec<String> {
        let lines = plain(editor, width);
        lines[1..lines.len() - 1].to_vec()
    }

    /// `editor.ts:123-216`.
    #[test]
    fn word_wrap_breaks_on_whitespace_and_keeps_chunk_indices() {
        let segments = char_segments("hello brave world");
        let chunks = word_wrap_line("hello brave world", 11, &segments);
        let texts: Vec<&str> = chunks.iter().map(|chunk| chunk.text.as_str()).collect();
        // pi's chunk keeps the trailing whitespace that made the break legal.
        assert_eq!(texts, vec!["hello ", "brave world"]);
        assert_eq!(chunks[0].start_index, 0);
        assert_eq!(chunks[0].end_index, 6);
        assert_eq!(chunks[1].start_index, 6);
        assert_eq!(chunks[1].end_index, 17);
    }

    #[test]
    fn word_wrap_breaks_between_cjk_characters() {
        let segments = char_segments("你好世界");
        let chunks = word_wrap_line("你好世界", 4, &segments);
        let texts: Vec<&str> = chunks.iter().map(|chunk| chunk.text.as_str()).collect();
        assert_eq!(texts, vec!["你好", "世界"]);
    }

    /// `editor.test.ts:1105-1141`: an oversized atomic segment is re-wrapped,
    /// and the chunks still tile the line exactly once.
    #[test]
    fn an_oversized_atomic_segment_is_split_without_being_duplicated() {
        let marker = "[paste #1 +20 lines]"; // 21 chars
        let line = format!("A{marker}");
        let segments = segment_with_markers(&line, &BTreeMap::from([(1, String::new())]));
        let chunks = word_wrap_line(&line, 10, &segments);
        for chunk in &chunks {
            assert!(
                visible_width(&chunk.text) <= 10,
                "chunk {:?} is {} wide",
                chunk.text,
                visible_width(&chunk.text)
            );
        }
        assert_eq!(chunks[0].text, "A");
        let reconstructed: String = chunks
            .iter()
            .map(|c| char_slice(&line, c.start_index, c.end_index).expect("in range"))
            .collect();
        assert_eq!(reconstructed, line, "the chunks tile the line exactly once");
    }

    /// Two markers on one line: the second is not at offset 0, and its
    /// detection must not slice past the tail.
    #[test]
    fn a_marker_that_is_not_at_the_start_is_still_merged() {
        let ids = BTreeMap::from([(1, String::new()), (2, String::new())]);
        assert_eq!(
            segment_with_markers("[paste #1 +20 lines][paste #2 +30 lines]", &ids)
                .into_iter()
                .map(|(_, text)| text)
                .collect::<Vec<_>>(),
            vec!["[paste #1 +20 lines]", "[paste #2 +30 lines]"]
        );
        assert_eq!(
            segment_with_markers("x[paste #1 +20 lines]", &ids)
                .into_iter()
                .map(|(_, t)| t)
                .collect::<Vec<_>>(),
            vec!["x", "[paste #1 +20 lines]"]
        );
    }

    /// `editor.test.ts:1143-1163`.
    #[test]
    fn consecutive_oversized_atomic_segments_are_both_split() {
        let m1 = "[paste #1 +20 lines]";
        let m2 = "[paste #2 +30 lines]";
        let line = format!("{m1}{m2}");
        let segments = segment_with_markers(
            &line,
            &BTreeMap::from([(1, String::new()), (2, String::new())]),
        );
        let chunks = word_wrap_line(&line, 10, &segments);
        for chunk in &chunks {
            assert!(
                visible_width(&chunk.text) <= 10,
                "chunk {:?} too wide",
                chunk.text
            );
        }
        let reconstructed: String = chunks
            .iter()
            .map(|c| char_slice(&line, c.start_index, c.end_index).expect("in range"))
            .collect();
        assert_eq!(reconstructed, line);
    }

    /// `editor.test.ts:1165-1190`: wrapping resumes normally afterwards.
    #[test]
    fn wrapping_resumes_after_an_oversized_atomic_segment() {
        let marker = "[paste #1 +20 lines]";
        let line = format!("{marker} hello world");
        let segments = segment_with_markers(&line, &BTreeMap::from([(1, String::new())]));
        let chunks = word_wrap_line(&line, 10, &segments);
        for chunk in &chunks {
            assert!(
                visible_width(&chunk.text) <= 10,
                "chunk {:?} too wide",
                chunk.text
            );
        }
        let reconstructed: String = chunks
            .iter()
            .map(|c| char_slice(&line, c.start_index, c.end_index).expect("in range"))
            .collect();
        assert_eq!(reconstructed, line);
        assert_eq!(chunks.last().expect("a tail").text, "world");
    }

    #[test]
    fn word_wrap_leaves_a_short_line_alone() {
        let segments = char_segments("short");
        let chunks = word_wrap_line("short", 10, &segments);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "short");
    }

    /// `editor.ts:1050-1069`.
    #[test]
    fn typing_and_backspace_edit_the_buffer() {
        let mut e = editor();
        type_text(&mut e, "ab");
        assert_eq!(e.get_text(), "ab");
        e.handle_input("\x7f");
        assert_eq!(e.get_text(), "a");
        assert_eq!(e.get_cursor(), (0, 1));
    }

    #[test]
    fn backspace_at_the_start_of_a_line_joins_it_to_the_previous() {
        let mut e = editor();
        e.set_text("one\ntwo");
        e.set_cursor(1, 0);
        e.handle_input(BACKSPACE);
        assert_eq!(e.get_text(), "onetwo");
        assert_eq!(e.get_cursor(), (0, 3));
    }

    #[test]
    fn left_arrow_at_the_start_wraps_to_the_previous_line() {
        let mut e = editor();
        e.set_text("one\ntwo");
        e.set_cursor(1, 2);
        e.handle_input(LEFT);
        assert_eq!(e.get_cursor(), (1, 1));
        e.handle_input(LEFT);
        e.handle_input(LEFT);
        assert_eq!(
            e.get_cursor(),
            (0, 3),
            "left past the line start wraps upward"
        );
    }

    #[test]
    fn right_arrow_past_the_end_moves_to_the_next_line() {
        let mut e = editor();
        e.set_text("ab\ncd");
        e.set_cursor(0, 2);
        e.handle_input(RIGHT);
        assert_eq!(e.get_cursor(), (1, 0));
    }

    #[test]
    fn word_movement_skips_to_word_starts() {
        let mut e = editor();
        e.set_text("alpha beta gamma");
        e.set_cursor(0, 16);
        // from the end, word-left lands on the last word's start
        let left = ALT_LEFT;
        let right = ALT_RIGHT;
        e.handle_input(left);
        assert_eq!(e.get_cursor(), (0, 11));
        e.handle_input(left);
        assert_eq!(e.get_cursor(), (0, 6));
        e.handle_input(right);
        assert_eq!(
            e.get_cursor(),
            (0, 10),
            "word-right stops at the next word's end"
        );
    }

    #[test]
    fn line_start_and_line_end_movement() {
        let mut e = editor();
        e.set_text("hello world");
        e.handle_input(HOME);
        assert_eq!(e.get_cursor(), (0, 0));
        e.handle_input(END);
        assert_eq!(e.get_cursor(), (0, 11));
    }

    #[test]
    fn shift_enter_inserts_a_newline_and_enter_submits() {
        let mut e = editor();
        type_text(&mut e, "ab");
        e.handle_input("\x1b[13;2~");
        assert_eq!(e.get_text(), "ab\n");
        type_text(&mut e, "cd");
        e.handle_input(ENTER);
        assert_eq!(e.take_submitted().as_deref(), Some("ab\ncd"));
        assert_eq!(e.get_text(), "");
    }

    #[test]
    fn submit_trims_and_clears() {
        let mut e = editor();
        type_text(&mut e, "  hi  ");
        e.handle_input("\r");
        assert_eq!(e.take_submitted().as_deref(), Some("hi"));
        assert!(e.is_empty());
    }

    #[test]
    fn disable_submit_swallows_enter() {
        let mut e = editor();
        e.disable_submit = true;
        type_text(&mut e, "hi");
        e.handle_input("\r");
        assert_eq!(e.take_submitted(), None);
        assert_eq!(e.get_text(), "hi");
    }

    /// `undo-stack.ts` / `editor.ts:2115-2134`: consecutive characters coalesce.
    #[test]
    fn undo_coalesces_typed_words() {
        let mut e = editor();
        type_text(&mut e, "hello");
        assert_eq!(e.undo_stack_depth(), 1);
        e.handle_input(CTRL_UNDERSCORE);
        assert_eq!(e.get_text(), "");
    }

    #[test]
    fn undo_restores_deleted_text() {
        let mut e = editor();
        type_text(&mut e, "ab");
        e.handle_input("\x7f");
        assert_eq!(e.get_text(), "a");
        e.handle_input(CTRL_UNDERSCORE);
        assert_eq!(e.get_text(), "ab");
    }

    #[test]
    fn undo_snapshots_the_multiline_shape() {
        let mut e = editor();
        e.set_text("one");
        type_text(&mut e, "!");
        e.handle_input(CTRL_UNDERSCORE);
        assert_eq!(e.get_text(), "one");
    }

    /// `kill-ring.ts` / `editor.ts:1659-1689`.
    #[test]
    fn kill_to_line_start_populates_the_ring_and_yank_restores() {
        let mut e = editor();
        e.set_text("hello world");
        e.set_cursor(0, 5);
        e.handle_input(CTRL_U);
        assert_eq!(e.get_text(), " world");
        assert_eq!(e.kill_ring().peek(), Some("hello"));
        e.handle_input(CTRL_Y);
        assert_eq!(e.get_text(), "hello world");
    }

    #[test]
    fn repeated_kills_concatenate_into_one_ring_entry() {
        let mut e = editor();
        e.set_text("aa bb cc");
        e.set_cursor(0, 8);
        e.handle_input(CTRL_U);
        e.set_cursor(0, 8);
        e.handle_input(CTRL_U);
        assert_eq!(e.get_text(), "");
        assert_eq!(e.kill_ring().len(), 1, "consecutive kills share one entry");
        assert_eq!(e.kill_ring().peek(), Some("aa bb cc"));
    }

    #[test]
    fn kill_word_backward_takes_a_whole_word() {
        let mut e = editor();
        e.set_text("alpha beta");
        e.handle_input(CTRL_W);
        assert_eq!(e.get_text(), "alpha ");
        assert_eq!(e.kill_ring().peek(), Some("beta"));
    }

    #[test]
    fn yank_pop_rotates_the_ring() {
        let mut e = editor();
        e.set_text("one two");
        e.set_cursor(0, 0);
        // Two kills separated by a cursor move, so the ring holds two entries.
        e.handle_input(ALT_D);
        assert_eq!(e.get_text(), " two", "alt+d kills up to the next word");
        assert_eq!(e.kill_ring().peek(), Some("one"));
        // A second, independent kill on a fresh buffer gives a second entry.
        e.set_text("three");
        e.set_cursor(0, 0);
        e.handle_input(ALT_D);
        assert_eq!(e.get_text(), "");
        assert_eq!(e.kill_ring().len(), 2);
        e.handle_input(CTRL_Y);
        assert_eq!(e.get_text(), "three", "yank takes the most recent kill");
        e.handle_input(ALT_Y);
        assert_eq!(e.get_text(), "one", "yank-pop rotates to the previous kill");
    }

    /// `editor.ts:455-491`.
    #[test]
    fn up_arrow_browses_history_and_returns_to_the_draft() {
        let mut e = editor();
        e.add_to_history("first");
        e.add_to_history("second");
        type_text(&mut e, "draft");
        let up = UP.to_string();
        e.handle_input(&up);
        assert_eq!(
            e.get_cursor(),
            (0, 0),
            "up on the first line goes to line start"
        );
        e.handle_input(&up);
        assert_eq!(e.get_text(), "second");
        e.handle_input(&up);
        assert_eq!(e.get_text(), "first");
        e.handle_input(DOWN);
        assert_eq!(e.get_text(), "second");
        e.handle_input(DOWN);
        assert_eq!(e.get_text(), "draft");
        assert_eq!(e.history_index(), -1);
    }

    #[test]
    fn history_add_ignores_blanks_and_immediate_repeats() {
        let mut e = editor();
        e.add_to_history("  ");
        e.add_to_history("a");
        e.add_to_history("a");
        e.add_to_history("b");
        assert_eq!(e.history(), ["b", "a"]);
    }

    #[test]
    fn typing_exits_history_browsing() {
        let mut e = editor();
        e.add_to_history("stored");
        e.handle_input(UP);
        assert_eq!(e.history_index(), 0);
        e.handle_input("x");
        assert_eq!(e.history_index(), -1);
    }

    /// `editor.ts:520-629`.
    #[test]
    fn render_draws_borders_and_a_reverse_video_cursor() {
        let mut e = editor();
        Focusable::set_focused(&mut e, true);
        type_text(&mut e, "hi");
        let lines = e.render_lines(20, 24);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(&"─".repeat(20)));
        assert!(lines[1].contains("\x1b[7m"));
        assert!(lines[1].contains(crate::ansi::CURSOR_MARKER));
        assert!(lines[2].contains(&"─".repeat(20)));
    }

    #[test]
    fn render_pads_the_body_to_the_content_width() {
        let mut e = editor();
        type_text(&mut e, "hi");
        let line = e.render_lines(20, 24).remove(1);
        assert_eq!(crate::ansi::visible_width(&line), 20);
    }

    #[test]
    fn a_long_line_wraps_onto_several_body_lines() {
        let mut e = editor();
        type_text(&mut e, "the quick brown fox jumps");
        let lines = body(&mut e, 12);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines, vec!["the quick", "brown fox", "jumps"]);
        assert_eq!(e.get_cursor(), (0, 25), "wrapping does not move the cursor");
    }

    #[test]
    fn scroll_indicators_appear_when_the_buffer_is_taller_than_the_window() {
        let mut e = editor();
        e.set_text("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        let lines = plain(&mut e, 12);
        assert!(lines[0].contains("more"), "{lines:?}");
    }

    #[test]
    fn a_focused_empty_editor_renders_a_reverse_video_space() {
        let mut e = editor();
        Focusable::set_focused(&mut e, true);
        let line = e.render_lines(10, 24).remove(1);
        assert!(line.contains("\x1b[7m \x1b[0m"), "{line:?}");
    }

    #[test]
    fn an_unfocused_editor_does_not_emit_the_cursor_marker() {
        let mut e = editor();
        type_text(&mut e, "hi");
        let line = e.render_lines(10, 24).remove(1);
        assert!(!line.contains(crate::ansi::CURSOR_MARKER), "{line:?}");
    }

    /// `editor.ts:1259-1325`.
    #[test]
    fn a_small_paste_is_inserted_inline() {
        let mut e = editor();
        type_text(&mut e, "x");
        e.handle_input("\x1b[200~hello\x1b[201~");
        assert_eq!(e.get_text(), "xhello");
    }

    #[test]
    fn a_large_paste_becomes_a_marker_and_expands_on_submit() {
        let mut e = editor();
        let big = "line\n".repeat(30);
        e.handle_input(&format!("\x1b[200~{big}\x1b[201~"));
        let text = e.get_text();
        assert!(text.starts_with("[paste #1 +31 lines]"), "{text}");
        assert_eq!(e.get_expanded_text(), big);
    }

    #[test]
    fn a_paste_in_chunks_accumulates_until_the_end_marker() {
        let mut e = editor();
        e.handle_input("\x1b[200~he");
        e.handle_input("llo\x1b[201~");
        assert_eq!(e.get_text(), "hello");
    }

    #[test]
    fn backspace_over_a_paste_marker_drops_the_registry_entry() {
        let mut e = editor();
        let big = "x".repeat(1200);
        e.handle_input(&format!("\x1b[200~{big}\x1b[201~"));
        assert!(e.get_text().starts_with("[paste #1"));
        e.handle_input("\x7f");
        assert_eq!(e.get_text(), "");
        assert!(e.pastes.is_empty());
    }

    #[test]
    fn set_text_normalizes_crlf_and_tabs() {
        let mut e = editor();
        e.set_text("a\r\nb\tc");
        assert_eq!(e.get_lines(), ["a", "b    c"]);
    }

    /// `editor.ts:2137-2165`.
    #[test]
    fn jump_mode_moves_to_the_next_occurrence() {
        let mut e = editor();
        e.set_text("banana");
        e.set_cursor(0, 0);
        let jump = CTRL_BRACKET_RIGHT;
        e.handle_input(jump);
        e.handle_input("n");
        assert_eq!(e.get_cursor(), (0, 2), "the first n after the cursor");
        e.handle_input(jump);
        e.handle_input("n");
        assert_eq!(
            e.get_cursor(),
            (0, 4),
            "jump mode stays armed until a printable key"
        );
        e.handle_input(jump);
        e.handle_input("z");
        assert_eq!(
            e.get_cursor(),
            (0, 4),
            "an absent character leaves the cursor put"
        );
    }

    #[test]
    fn escape_cancels_jump_mode() {
        let mut e = editor();
        e.set_text("banana");
        let jump = CTRL_BRACKET_RIGHT;
        e.handle_input(jump);
        e.handle_input("\x1b");
        assert_eq!(e.get_cursor(), (0, 6));
    }

    /// `editor.ts:1580-1611`.
    #[test]
    fn vertical_movement_keeps_a_sticky_column() {
        let mut e = editor();
        e.set_text("longest line here\nab\nanother long line");
        let up = UP.to_string();
        let down = DOWN.to_string();
        e.set_cursor(2, 9);
        e.handle_input(&up);
        assert_eq!(e.get_cursor(), (1, 2), "the short line clamps the column");
        e.handle_input(&up);
        assert_eq!(e.get_cursor(), (0, 9), "the remembered column is restored");
        e.handle_input(&down);
        assert_eq!(e.get_cursor(), (1, 2));
        e.handle_input(&down);
        assert_eq!(e.get_cursor(), (2, 9));
    }

    #[test]
    fn up_arrow_on_the_first_line_of_a_buffer_goes_to_history() {
        let mut e = editor();
        e.add_to_history("old");
        type_text(&mut e, "draft");
        e.handle_input(HOME);
        e.handle_input(UP);
        assert_eq!(e.get_text(), "old");
    }

    #[test]
    fn placeholder_is_readable_when_set() {
        let mut e = editor();
        assert_eq!(e.placeholder(), None);
        e.set_placeholder(Some("Type a message".to_string()));
        assert_eq!(e.placeholder(), Some("Type a message"));
    }

    #[test]
    fn padding_is_clamped_to_half_the_width() {
        let mut e = editor();
        e.set_padding_x(100);
        let line = e.render_lines(11, 24).remove(1);
        assert_eq!(crate::ansi::visible_width(&line), 11);
        assert!(
            line.starts_with("     "),
            "five columns of padding: {line:?}"
        );
    }

    // ------------------------------------------------------------ autocomplete

    struct FileProvider;

    impl AutocompleteProvider for FileProvider {
        fn trigger_characters(&self) -> Vec<char> {
            vec!['@']
        }
        fn suggest(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            _force: bool,
        ) -> Option<Suggestions> {
            let line = lines.get(cursor_line)?;
            let before = char_slice(line, 0, cursor_col)?;
            let start = before.rfind('@')?;
            let prefix = &before[start + 1..];
            if prefix.contains(' ') {
                return None;
            }
            Some(Suggestions {
                items: vec![
                    SelectItem::new("src/main.rs", "src/main.rs"),
                    SelectItem::new("src/lib.rs", "src/lib.rs"),
                    SelectItem::new("Cargo.toml", "Cargo.toml"),
                ]
                .into_iter()
                .filter(|item| item.value.starts_with(prefix))
                .collect(),
                prefix: prefix.to_string(),
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            item: &SelectItem,
            prefix: &str,
        ) -> (Vec<String>, usize, usize) {
            let mut next = lines.to_vec();
            let line = next[cursor_line].clone();
            let end = char_slice(&line, cursor_col, line.chars().count())
                .unwrap_or("")
                .to_string();
            let kept = char_slice(&line, 0, cursor_col.saturating_sub(prefix.chars().count()))
                .unwrap_or("")
                .to_string();
            next[cursor_line] = format!("{kept}{} {end}", item.value);
            let col = kept.chars().count() + item.value.chars().count() + 1;
            (next, cursor_line, col)
        }
    }

    #[test]
    fn typing_a_trigger_opens_the_provider_list() {
        let mut e = editor();
        e.set_autocomplete_provider(Some(Arc::new(FileProvider)));
        type_text(&mut e, "@sr");
        assert!(e.is_showing_autocomplete());
        let lines = plain(&mut e, 40);
        assert!(
            lines.iter().any(|line| line.contains("src/lib.rs")),
            "{lines:?}"
        );
    }

    #[test]
    fn tab_applies_the_best_match_and_closes_the_list() {
        let mut e = editor();
        e.set_autocomplete_provider(Some(Arc::new(FileProvider)));
        type_text(&mut e, "@sr");
        e.handle_input("\t");
        assert!(!e.is_showing_autocomplete());
        assert_eq!(e.get_text(), "@src/main.rs ");
    }

    #[test]
    fn escape_cancels_the_autocomplete_list() {
        let mut e = editor();
        e.set_autocomplete_provider(Some(Arc::new(FileProvider)));
        type_text(&mut e, "@sr");
        e.handle_input("\x1b");
        assert!(!e.is_showing_autocomplete());
        assert_eq!(e.get_text(), "@sr");
    }

    #[test]
    fn autocomplete_only_triggers_at_a_token_boundary() {
        let mut e = editor();
        e.set_autocomplete_provider(Some(Arc::new(FileProvider)));
        type_text(&mut e, "mail@sr");
        assert!(!e.is_showing_autocomplete());
    }

    #[test]
    fn an_exact_prefix_match_is_selected_first() {
        let items = vec![
            SelectItem::new("alpha", "alpha"),
            SelectItem::new("alphabet", "alphabet"),
        ];
        assert_eq!(Editor::best_match_index(&items, "alpha"), Some(0));
        assert_eq!(Editor::best_match_index(&items, "alphabet"), Some(1));
        assert_eq!(Editor::best_match_index(&items, "zzz"), None);
        assert_eq!(Editor::best_match_index(&items, ""), None);
    }

    /// `editor.ts:1200-1229` plus the autocompletion path: every edit re-asks.
    #[test]
    fn the_list_follows_the_typed_prefix() {
        let mut e = editor();
        e.set_autocomplete_provider(Some(Arc::new(FileProvider)));
        type_text(&mut e, "@C");
        assert!(e.is_showing_autocomplete());
        let lines = plain(&mut e, 40);
        assert!(
            lines.iter().any(|line| line.contains("Cargo.toml")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("src/lib.rs")),
            "{lines:?}"
        );
    }
}
