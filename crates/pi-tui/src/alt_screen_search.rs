//! Transcript search for the alt-screen TUI: the searchable corpus, the match
//! finder with its index cache, and the three-line search overlay component.
//!
//! Ground truth: `packages/tui/src/alt-screen-search.ts` (327 lines).
//!
//! - corpus + matching + index cache: `alt-screen-search.ts:8-195`
//! - overlay component: `alt-screen-search.ts:197-327`
//!
//! Two pi details differ in the port. pi matches with a `giu` `RegExp`; `regex`
//! is not a workspace dependency, so [`find_search_corpus_matches`] lowercases
//! both sides and scans for the literal query — equivalent for the queries a
//! transcript search sees. pi's component reads the keybindings inside
//! `render`; the port takes the previous/next key labels as options so a
//! caller can override them, and its own defaults are pi's
//! `tui.altScreen.searchPrevious` / `tui.altScreen.searchNext`
//! (`keybindings.ts:194-199`).

use std::cell::Cell;
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::{strip_terminal_sequences, truncate_to_width, visible_width};
use crate::component::{Component, ComponentMutation};
use crate::components::input::{Input, InputOptions};
use crate::keybindings::keys_for;

/// One contiguous run of searchable text on one rendered row.
/// `alt-screen-search.ts:8-15`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSourceSpan {
    /// Byte offset of the run in [`SearchCorpus::text`].
    pub text_start: usize,
    pub text_end: usize,
    /// Rendered row the run came from.
    pub row: usize,
    /// Rendered column range the run occupies.
    pub start_col: usize,
    pub end_col: usize,
    /// `true` when the run is printable ASCII, so columns are `column + n`
    /// rather than grapheme widths. `alt-screen-search.ts:32`.
    pub linear_columns: bool,
}

/// Flattened searchable text plus the mapping back to rendered cells.
/// `alt-screen-search.ts:17-20`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCorpus {
    pub text: String,
    pub spans: Vec<SearchSourceSpan>,
    /// `text` lowercased once at build time, since every search lowercases.
    lower_text: String,
    /// `lower_to_text[i]` is the byte offset in `text` for byte offset `i` of
    /// `lower_text`; the vector has `lower_text.len() + 1` entries.
    lower_to_text: Vec<usize>,
}

/// A run of matched cells on one row. `alt-screen-search.ts:22-25`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AltScreenSearchSegment {
    pub row: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// One query hit, possibly spanning rows. `alt-screen-search.ts:27-29`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltScreenSearchMatch {
    pub segments: Vec<AltScreenSearchSegment>,
}

/// `buildSearchCorpus`, `alt-screen-search.ts:34-104`.
///
/// Whitespace never becomes a span: it only sets `pendingSeparator`, so the
/// joined text has exactly one space between runs, including across rows
/// (`alt-screen-search.ts:39-45`, `:97-98`).
pub fn build_search_corpus(lines: &[String]) -> SearchCorpus {
    let mut chunks: Vec<String> = Vec::new();
    let mut spans: Vec<SearchSourceSpan> = Vec::new();
    let mut text_length = 0usize;
    let mut pending_separator = false;

    macro_rules! append_separator {
        () => {
            if pending_separator {
                chunks.push(" ".to_string());
                text_length += 1;
                pending_separator = false;
            }
        };
    }

    for (row, line) in lines.iter().enumerate() {
        let line = strip_terminal_sequences(line);
        let mut column = 0usize;

        // Rendered transcripts are overwhelmingly ASCII. Index complete
        // non-space runs at once instead of allocating one mapping per cell.
        // `alt-screen-search.ts:50-52`.
        if line.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
            let bytes = line.as_bytes();
            let mut index = 0usize;
            while index < bytes.len() {
                if bytes[index] == 0x20 {
                    if text_length > 0 {
                        pending_separator = true;
                    }
                    column += 1;
                    index += 1;
                    continue;
                }
                let mut end = index + 1;
                while end < bytes.len() && bytes[end] != 0x20 {
                    end += 1;
                }
                append_separator!();
                let text = &line[index..end];
                chunks.push(text.to_string());
                spans.push(SearchSourceSpan {
                    text_start: text_length,
                    text_end: text_length + text.len(),
                    row,
                    start_col: column,
                    end_col: column + text.len(),
                    linear_columns: true,
                });
                text_length += text.len();
                column += text.len();
                index = end;
            }
        } else {
            for grapheme in line.graphemes(true) {
                let width = visible_width(grapheme);
                if !grapheme.is_empty() && grapheme.chars().all(char::is_whitespace) {
                    if text_length > 0 {
                        pending_separator = true;
                    }
                    column += width;
                    continue;
                }
                append_separator!();
                chunks.push(grapheme.to_string());
                spans.push(SearchSourceSpan {
                    text_start: text_length,
                    text_end: text_length + grapheme.len(),
                    row,
                    start_col: column,
                    end_col: column + width,
                    linear_columns: false,
                });
                text_length += grapheme.len();
                column += width;
            }
        }
        if text_length > 0 {
            pending_separator = true;
        }
    }

    let text = chunks.concat();
    let (lower_text, lower_to_text) = lowercase_with_offset_map(&text);
    SearchCorpus {
        text,
        spans,
        lower_text,
        lower_to_text,
    }
}

/// Lowercase `text` and record, for every byte offset of the result, the byte
/// offset of the original it came from.
///
/// A few characters lowercase to more bytes than they occupy (`İ`), so the
/// offsets inside such an expansion have no exact original position; they map
/// to the end of the original character so span boundaries always land on one.
fn lowercase_with_offset_map(text: &str) -> (String, Vec<usize>) {
    let mut lower = String::with_capacity(text.len());
    let mut map: Vec<usize> = Vec::with_capacity(text.len() + 1);
    for (offset, ch) in text.char_indices() {
        let mut first = true;
        for lowered in ch.to_lowercase() {
            // One entry per *byte* of the lowered char: the offsets are
            // consumed as byte offsets into `lower_text`.
            let tail = offset + ch.len_utf8();
            for _ in 0..lowered.len_utf8() {
                map.push(if first { offset } else { tail });
                first = false;
            }
            lower.push(lowered);
        }
    }
    map.push(text.len());
    (lower, map)
}

/// `normalizeQuery`, `alt-screen-search.ts:107-110`: whitespace runs collapse
/// to one space and the ends are trimmed.
pub fn normalize_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `findSearchCorpusMatches`, `alt-screen-search.ts:125-148`. The query is
/// already normalized; it is lowercased here because there is no `iu` flag.
pub fn find_search_corpus_matches(
    corpus: &SearchCorpus,
    normalized_query: &str,
) -> Vec<AltScreenSearchMatch> {
    let query = normalized_query.to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }

    let mut matches: Vec<AltScreenSearchMatch> = Vec::new();
    let mut span_index = 0usize;
    let haystack = corpus.lower_text.as_str();
    let mut search_from = 0usize;

    while let Some(found) = haystack[search_from..].find(query.as_str()) {
        let lower_start = search_from + found;
        let lower_end = lower_start + query.len();
        let start = corpus.lower_to_text[lower_start];
        let end = corpus.lower_to_text[lower_end];
        // `matchAll` does not produce overlapping hits.
        search_from = lower_end.max(lower_start + 1);

        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= start {
            span_index += 1;
        }

        let mut segments: Vec<AltScreenSearchSegment> = Vec::new();
        for span in corpus.spans.iter().skip(span_index) {
            if span.text_start >= end {
                break;
            }
            if span.text_end <= start {
                continue;
            }
            let start_col = if span.linear_columns {
                span.start_col + start.max(span.text_start) - span.text_start
            } else {
                span.start_col
            };
            let end_col = if span.linear_columns {
                span.start_col + end.min(span.text_end) - span.text_start
            } else {
                span.end_col
            };
            match segments.last_mut() {
                // Adjacent spans on one row collapse into one highlight run.
                Some(previous) if previous.row == span.row && start_col <= previous.end_col => {
                    previous.end_col = previous.end_col.max(end_col);
                }
                _ => segments.push(AltScreenSearchSegment {
                    row: span.row,
                    start_col,
                    end_col,
                }),
            }
        }
        while span_index < corpus.spans.len() && corpus.spans[span_index].text_end <= end {
            span_index += 1;
        }
        if !segments.is_empty() {
            matches.push(AltScreenSearchMatch { segments });
        }
    }

    matches
}

/// Cached matches plus whether they changed. `alt-screen-search.ts:150-154`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltScreenSearchResult {
    pub matches: Vec<AltScreenSearchMatch>,
    pub changed: bool,
}

/// Caches the corpus and the matches while the rendered lines and the query
/// are unchanged. `alt-screen-search.ts:156-184`.
#[derive(Debug, Default)]
pub struct AltScreenSearchIndex {
    source_lines: Vec<String>,
    corpus: Option<SearchCorpus>,
    normalized_query: Option<String>,
    matches: Vec<AltScreenSearchMatch>,
}

impl AltScreenSearchIndex {
    /// An empty index; the first [`AltScreenSearchIndex::search`] builds the corpus.
    pub fn new() -> Self {
        Self::default()
    }

    /// `AltScreenSearchIndex.search`, `alt-screen-search.ts:160-183`.
    pub fn search(&mut self, lines: &[String], query: &str) -> AltScreenSearchResult {
        let source_changed = self.source_lines.len() != lines.len()
            || lines
                .iter()
                .enumerate()
                .any(|(index, line)| self.source_lines.get(index) != Some(line));
        if source_changed || self.corpus.is_none() {
            self.source_lines = lines.to_vec();
            self.corpus = Some(build_search_corpus(lines));
        }

        let normalized_query = normalize_query(query);
        let changed = source_changed || Some(&normalized_query) != self.normalized_query.as_ref();
        if changed {
            self.normalized_query = Some(normalized_query.clone());
            let corpus = self.corpus.as_ref().expect("corpus built above");
            self.matches = find_search_corpus_matches(corpus, &normalized_query);
        }
        AltScreenSearchResult {
            matches: self.matches.clone(),
            changed,
        }
    }
}

/// `findAltScreenSearchMatches`, `alt-screen-search.ts:186-189`.
pub fn find_alt_screen_search_matches(lines: &[String], query: &str) -> Vec<AltScreenSearchMatch> {
    let normalized_query = normalize_query(query);
    if normalized_query.is_empty() {
        return Vec::new();
    }
    find_search_corpus_matches(&build_search_corpus(lines), &normalized_query)
}

/// `getAltScreenSearchMatchKey`, `alt-screen-search.ts:191-195`. Identity for
/// re-anchoring the selection across re-renders.
pub fn get_alt_screen_search_match_key(found: &AltScreenSearchMatch) -> String {
    match (found.segments.first(), found.segments.last()) {
        (Some(first), Some(last)) => {
            format!(
                "{}:{}:{}:{}",
                first.row, first.start_col, last.row, last.end_col
            )
        }
        _ => String::new(),
    }
}

/// Which arrow button the pointer is over. `-1` is previous, `1` is next, so
/// the caller can add it to a match index directly.
pub type SearchNavigationDirection = i8;

/// `(text, hovered) -> styled text`; the identity default is pi's.
/// `navigationButtonStyle`, `alt-screen-search.ts:211`.
pub type SearchButtonStyle = Arc<dyn Fn(&str, bool) -> String + Send + Sync>;

/// `onQueryChange`, `alt-screen-search.ts:206`.
pub type SearchQueryChangeHandler = Arc<dyn Fn(&str) + Send + Sync>;

/// `const PREVIOUS = -1` / `NEXT = 1`, `alt-screen-search.ts:225`.
pub const SEARCH_DIRECTION_PREVIOUS: SearchNavigationDirection = -1;
/// The "next match" button: `+1`, addable to a match index.
pub const SEARCH_DIRECTION_NEXT: SearchNavigationDirection = 1;

/// How the overlay labels a key. `alt-screen-search.ts:252-262`.
pub fn format_search_key(key: Option<&str>) -> String {
    let Some(key) = key else {
        return "Unbound".to_string();
    };
    key.split('+')
        .map(|part| {
            // pi renders "alt" as "Option" on macOS.
            if cfg!(target_os = "macos") && part.eq_ignore_ascii_case("alt") {
                return "Option".to_string();
            }
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// `AltScreenSearchComponentOptions`. pi takes `onQueryChange` and
/// `navigationButtonStyle` as constructor arguments
/// (`alt-screen-search.ts:206-215`); the key labels are added because the port
/// reads no keybindings inside `render`.
#[derive(Clone)]
pub struct AltScreenSearchComponentOptions {
    pub on_query_change: SearchQueryChangeHandler,
    /// `(text, hovered) -> styled text`; the identity by default, as in pi.
    pub navigation_button_style: SearchButtonStyle,
    /// First key bound to `tui.altScreen.searchPrevious`.
    pub previous_key: Option<String>,
    /// First key bound to `tui.altScreen.searchNext`.
    pub next_key: Option<String>,
}

impl AltScreenSearchComponentOptions {
    /// Options with pi's defaults: identity button style, no navigation keys.
    pub fn new(on_query_change: SearchQueryChangeHandler) -> Self {
        Self {
            on_query_change,
            navigation_button_style: Arc::new(|text: &str, _hovered: bool| text.to_string()),
            previous_key: None,
            next_key: None,
        }
    }

    /// Fill the button labels from `keybindings.rs` when the `tui.altScreen.*`
    /// ids land; until then they stay `None` and the buttons show "Unbound".
    #[must_use]
    pub fn with_keybindings(mut self) -> Self {
        let first = |keys: &'static [&'static str]| keys.first().map(ToString::to_string);
        self.previous_key = first(keys_for("tui.altScreen.searchPrevious"));
        self.next_key = first(keys_for("tui.altScreen.searchNext"));
        self
    }
}

impl Default for AltScreenSearchComponentOptions {
    fn default() -> Self {
        Self::new(Arc::new(|_query: &str| {}))
    }
}

impl std::fmt::Debug for AltScreenSearchComponentOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AltScreenSearchComponentOptions")
            .field("previous_key", &self.previous_key)
            .field("next_key", &self.next_key)
            .finish_non_exhaustive()
    }
}

/// `AltScreenSearchComponent`, `alt-screen-search.ts:197-327`. Three lines
/// wide: a rule, the input with a right-aligned result counter, and a rule
/// with the two hoverable navigation buttons.
pub struct AltScreenSearchComponent {
    input: Input,
    on_query_change: SearchQueryChangeHandler,
    navigation_button_style: SearchButtonStyle,
    previous_key: Option<String>,
    next_key: Option<String>,
    result_count: usize,
    result_index: i64,
    /// Recorded during `render` so the caller can hit-test hover and clicks;
    /// pi stores the same four numbers (`alt-screen-search.ts:214-217`).
    previous_button_start: Cell<i64>,
    previous_button_end: Cell<i64>,
    next_button_start: Cell<i64>,
    next_button_end: Cell<i64>,
    hovered_navigation_direction: Option<SearchNavigationDirection>,
    focused: bool,
}

impl std::fmt::Debug for AltScreenSearchComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AltScreenSearchComponent")
            .field("query", &self.input.value())
            .field("result_index", &self.result_index)
            .field("result_count", &self.result_count)
            .finish_non_exhaustive()
    }
}

impl AltScreenSearchComponent {
    /// `alt-screen-search.ts:198-215`.
    pub fn new(mut options: AltScreenSearchComponentOptions) -> Self {
        let input = Input::new(InputOptions {
            prompt: " ".to_string(),
            placeholder: "Find in transcript".to_string(),
            placeholder_style: Arc::new(|text: &str| format!("\x1b[2m{text}\x1b[22m")),
        });
        let previous_key = options.previous_key.take();
        let next_key = options.next_key.take();
        Self {
            input,
            on_query_change: options.on_query_change,
            navigation_button_style: options.navigation_button_style,
            previous_key,
            next_key,
            result_count: 0,
            result_index: -1,
            previous_button_start: Cell::new(-1),
            previous_button_end: Cell::new(-1),
            next_button_start: Cell::new(-1),
            next_button_end: Cell::new(-1),
            hovered_navigation_direction: None,
            focused: false,
        }
    }

    /// The current query text.
    pub fn value(&self) -> &str {
        self.input.value()
    }

    /// Replace the query; notifies only when the value actually changed.
    pub fn set_value(&mut self, value: impl Into<String>) {
        let previous = self.input.value().to_string();
        self.input.set_value(value);
        let query = self.input.value().to_string();
        if query != previous {
            (self.on_query_change)(&query);
        }
    }

    /// Whether the overlay currently has keyboard focus.
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Replace the button labels after the host binds or rebinds keys.
    pub fn set_navigation_keys(&mut self, previous: Option<String>, next: Option<String>) {
        self.previous_key = previous;
        self.next_key = next;
    }

    /// `setResult`, `alt-screen-search.ts:216-219`.
    pub fn set_result(&mut self, index: i64, count: usize) {
        self.result_index = index;
        self.result_count = count;
    }

    /// `getNavigationDirectionAt`, `alt-screen-search.ts:221-226`. Only the
    /// button row counts; the separator between the buttons is dead space.
    pub fn get_navigation_direction_at(
        &self,
        row: usize,
        column: i64,
    ) -> Option<SearchNavigationDirection> {
        if row != 2 {
            return None;
        }
        if column >= self.previous_button_start.get() && column < self.previous_button_end.get() {
            return Some(SEARCH_DIRECTION_PREVIOUS);
        }
        if column >= self.next_button_start.get() && column < self.next_button_end.get() {
            return Some(SEARCH_DIRECTION_NEXT);
        }
        None
    }

    /// The recorded button column ranges, as `(previous, next)`.
    pub fn navigation_button_ranges(&self) -> ((i64, i64), (i64, i64)) {
        (
            (
                self.previous_button_start.get(),
                self.previous_button_end.get(),
            ),
            (self.next_button_start.get(), self.next_button_end.get()),
        )
    }

    /// `setHoveredNavigationDirection`, `alt-screen-search.ts:228-232`.
    /// Returns whether the hover changed, which is what drives the re-render.
    pub fn set_hovered_navigation_direction(
        &mut self,
        direction: Option<SearchNavigationDirection>,
    ) -> bool {
        if direction == self.hovered_navigation_direction {
            return false;
        }
        self.hovered_navigation_direction = direction;
        true
    }

    /// Which navigation button is hovered, if either.
    pub fn hovered_navigation_direction(&self) -> Option<SearchNavigationDirection> {
        self.hovered_navigation_direction
    }

    /// The result text of the middle line. `alt-screen-search.ts:263-268`.
    fn result_text(&self) -> String {
        if self.input.value().is_empty() {
            String::new()
        } else if self.result_count == 0 {
            "No matches".to_string()
        } else {
            format!("{}/{}", self.result_index + 1, self.result_count)
        }
    }
}

impl Component for AltScreenSearchComponent {
    /// The alt screen reaches the search component by typed reference
    /// (`tui-alt-screen.ts:512-517`); the overlay owns it by value here.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    /// `render`, `alt-screen-search.ts:246-321`.
    // `outer_gap_width`/`outer_gaps_width` mirror the upstream naming split
    // between the gap and the combined pair.
    #[allow(clippy::similar_names)]
    fn render(&self, width: usize) -> Vec<String> {
        let safe_width = width.max(1);
        let inner_width = safe_width.saturating_sub(2);

        let previous_key = format_search_key(self.previous_key.as_deref());
        let next_key = format_search_key(self.next_key.as_deref());

        let result_space = inner_width.saturating_sub(3);
        let visible_result = truncate_to_width(&self.result_text(), result_space, "", false);
        let result_text = if visible_result.is_empty() {
            String::new()
        } else {
            format!("\x1b[2m {visible_result} \x1b[22m")
        };
        let input_width = inner_width.saturating_sub(visible_width(&result_text));
        let input_line = self
            .input
            .render(input_width.max(1))
            .first()
            .cloned()
            .unwrap_or_default();
        let input_line = truncate_to_width(&input_line, input_width, "", false);
        let input_padding = " ".repeat(input_width.saturating_sub(visible_width(&input_line)));
        let content = format!("{input_line}{input_padding}{result_text}");

        let mut previous_button = format!("↑ {previous_key}");
        let mut next_button = format!("↓ {next_key}");
        let mut separator = " · ";
        let outer_gap_width = 1usize;
        let available_controls_width = inner_width.saturating_sub(outer_gap_width * 2 + 1);
        let mut controls_width = visible_width(&previous_button)
            + visible_width(separator)
            + visible_width(&next_button);
        if controls_width > available_controls_width {
            previous_button = "↑".to_string();
            next_button = "↓".to_string();
            separator = " ";
            controls_width = visible_width(&previous_button)
                + visible_width(separator)
                + visible_width(&next_button);
        }
        let show_buttons = controls_width <= available_controls_width;
        let rendered_buttons = if show_buttons {
            format!(
                "{}{}{}",
                (self.navigation_button_style)(
                    &previous_button,
                    self.hovered_navigation_direction == Some(-1)
                ),
                separator,
                (self.navigation_button_style)(
                    &next_button,
                    self.hovered_navigation_direction == Some(1)
                ),
            )
        } else {
            String::new()
        };
        let outer_gaps_width = if show_buttons { outer_gap_width * 2 } else { 0 };
        let right_rule_width = usize::from(
            !rendered_buttons.is_empty() && inner_width > controls_width + outer_gaps_width,
        );
        let left_rule_width = inner_width.saturating_sub(
            if show_buttons { controls_width } else { 0 } + outer_gaps_width + right_rule_width,
        );
        let previous_start = 1 + left_rule_width + outer_gap_width;
        self.previous_button_start.set(if show_buttons {
            previous_start as i64
        } else {
            -1
        });
        self.previous_button_end.set(if show_buttons {
            (previous_start + visible_width(&previous_button)) as i64
        } else {
            -1
        });
        let next_start =
            previous_start + visible_width(&previous_button) + visible_width(separator);
        self.next_button_start
            .set(if show_buttons { next_start as i64 } else { -1 });
        self.next_button_end.set(if show_buttons {
            (next_start + visible_width(&next_button)) as i64
        } else {
            -1
        });

        if safe_width == 1 {
            return vec!["┌".to_string(), "│".to_string(), "└".to_string()];
        }
        vec![
            format!("┌{}┐", "─".repeat(inner_width)),
            format!("│{content}│"),
            // `alt-screen-search.ts:319`: left rule, one gap, the styled buttons,
            // one gap, the right rule. The gaps exist only when buttons render.
            format!(
                "└{}{}{}{}{}┘",
                "─".repeat(left_rule_width),
                if rendered_buttons.is_empty() { "" } else { " " },
                rendered_buttons,
                if rendered_buttons.is_empty() { "" } else { " " },
                "─".repeat(right_rule_width),
            ),
        ]
    }

    /// `handleInput`, `alt-screen-search.ts:234-239`: forward to the input and
    /// notify only when the value actually changed.
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        let previous = self.input.value().to_string();
        self.input.handle_key(data);
        let query = self.input.value().to_string();
        if query != previous {
            (self.on_query_change)(&query);
        }
        Some(ComponentMutation { consume: true })
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    fn invalidate(&mut self) {
        self.input.invalidate();
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        crate::component::Component::set_focused(&mut self.input, focused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn lines(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    /// `MouseEvent`-free helper for the hit-test assertions: column of the
    /// first occurrence of `needle` in a rendered line.
    fn index_of(haystack: &str, needle: &str) -> i64 {
        let plain = strip_terminal_sequences(haystack);
        let byte = plain.find(needle).expect("needle present");
        visible_width(&plain[..byte]) as i64
    }

    fn last_index_of(haystack: &str, needle: &str) -> i64 {
        let plain = strip_terminal_sequences(haystack);
        let byte = plain.rfind(needle).expect("needle present");
        visible_width(&plain[..byte]) as i64
    }

    // B2.1 — corpus: one span per ASCII run, separator collapsing across rows.
    #[test]
    fn corpus_indexes_ascii_runs_and_collapses_row_separators() {
        let corpus = build_search_corpus(&lines(&["alpha  beta", "  gamma"]));
        assert_eq!(corpus.text, "alpha beta gamma");
        assert_eq!(corpus.spans.len(), 3);
        assert_eq!(
            corpus.spans[0],
            SearchSourceSpan {
                text_start: 0,
                text_end: 5,
                row: 0,
                start_col: 0,
                end_col: 5,
                linear_columns: true,
            }
        );
        assert_eq!((corpus.spans[1].row, corpus.spans[1].start_col), (0, 7));
        assert_eq!((corpus.spans[2].row, corpus.spans[2].start_col), (1, 2));
    }

    // B2.1 — corpus: grapheme fallback uses visible widths, one span per grapheme.
    #[test]
    fn corpus_falls_back_to_graphemes_with_width_columns() {
        let corpus = build_search_corpus(&lines(&["A界🙂éZ"]));
        let cols: Vec<(usize, usize, bool)> = corpus
            .spans
            .iter()
            .map(|span| (span.start_col, span.end_col, span.linear_columns))
            .collect();
        assert_eq!(
            cols,
            vec![
                (0, 1, false),
                (1, 3, false),
                (3, 5, false),
                (5, 6, false),
                (6, 7, false)
            ]
        );
        assert_eq!(corpus.text, "A界🙂éZ");
    }

    // B2.2 — normalizeQuery.
    #[test]
    fn normalize_query_collapses_whitespace_and_trims() {
        assert_eq!(normalize_query("  quick  brown \n fox "), "quick brown fox");
        assert_eq!(normalize_query(""), "");
        assert_eq!(normalize_query("   "), "");
    }

    // B2.3 / criterion 12 — normalized text matches across rows.
    #[test]
    fn searches_normalized_text_across_rows() {
        let matches =
            find_alt_screen_search_matches(&lines(&["alpha QUICK", "brown fox"]), "quick brown");
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].segments,
            vec![
                AltScreenSearchSegment {
                    row: 0,
                    start_col: 6,
                    end_col: 11
                },
                AltScreenSearchSegment {
                    row: 1,
                    start_col: 0,
                    end_col: 5
                },
            ]
        );
    }

    // B2.3 / criterion 13 — ANSI stripping, run merging, Unicode column mapping.
    #[test]
    fn maps_ascii_and_unicode_matches_back_to_rendered_columns() {
        let matches = find_alt_screen_search_matches(
            &lines(&["\x1b[31mfoo  bar\x1b[0m", "A界🙂éZ"]),
            "oo   bar\nA界🙂é",
        );
        assert_eq!(
            matches,
            vec![AltScreenSearchMatch {
                segments: vec![
                    AltScreenSearchSegment {
                        row: 0,
                        start_col: 1,
                        end_col: 3
                    },
                    AltScreenSearchSegment {
                        row: 0,
                        start_col: 5,
                        end_col: 8
                    },
                    AltScreenSearchSegment {
                        row: 1,
                        start_col: 0,
                        end_col: 6
                    },
                ],
            }]
        );
    }

    #[test]
    fn empty_query_and_no_hit_return_no_matches() {
        assert!(find_alt_screen_search_matches(&lines(&["alpha"]), "  ").is_empty());
        assert!(find_alt_screen_search_matches(&lines(&["alpha"]), "zzz").is_empty());
    }

    #[test]
    fn matching_is_case_insensitive_but_non_overlapping() {
        let matches = find_alt_screen_search_matches(&lines(&["aaaa"]), "aa");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[1].segments[0].start_col, 2);
    }

    // B2.4 / criterion 14 — the index only recomputes when the source or query changes.
    #[test]
    fn index_reuses_matches_until_source_or_query_changes() {
        let mut index = AltScreenSearchIndex::new();
        let source = lines(&["one", "two"]);

        let first = index.search(&source, "two");
        assert!(first.changed);
        assert_eq!(first.matches.len(), 1);

        let repeat = index.search(&source, "two");
        assert!(!repeat.changed);
        assert_eq!(repeat.matches, first.matches);

        let next_query = index.search(&source, "one");
        assert!(next_query.changed);
        assert_eq!(
            next_query.matches,
            vec![AltScreenSearchMatch {
                segments: vec![AltScreenSearchSegment {
                    row: 0,
                    start_col: 0,
                    end_col: 3
                }],
            }]
        );

        let changed_lines = index.search(&lines(&["one", "zzz"]), "one");
        assert!(changed_lines.changed);
        assert_eq!(changed_lines.matches.len(), 1);
        let dropped = index.search(&lines(&["zzz", "zzz"]), "one");
        assert!(dropped.changed);
        assert!(dropped.matches.is_empty());
    }

    // B2.5 — match key identity.
    #[test]
    fn match_key_spans_first_to_last_segment() {
        let matches =
            find_alt_screen_search_matches(&lines(&["alpha QUICK", "brown fox"]), "quick brown");
        assert_eq!(get_alt_screen_search_match_key(&matches[0]), "0:6:1:5");
        assert_eq!(
            get_alt_screen_search_match_key(&AltScreenSearchMatch { segments: vec![] }),
            ""
        );
    }

    #[test]
    fn format_key_capitalizes_parts_and_reports_unbound() {
        assert_eq!(format_search_key(Some("shift+enter")), "Shift+Enter");
        assert_eq!(format_search_key(Some("enter")), "Enter");
        assert_eq!(format_search_key(None), "Unbound");
    }

    fn component_with_recorded_query(
        keys: Option<(&str, &str)>,
    ) -> (AltScreenSearchComponent, Arc<Mutex<Vec<String>>>) {
        let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        let mut options = AltScreenSearchComponentOptions::new(Arc::new(move |query: &str| {
            sink.lock().expect("lock").push(query.to_string());
        }));
        if let Some((previous, next)) = keys {
            options.previous_key = Some(previous.to_string());
            options.next_key = Some(next.to_string());
        }
        (AltScreenSearchComponent::new(options), recorded)
    }

    // B2.6 / criterion 15 — the three rendered lines at width 48.
    #[test]
    fn renders_placeholder_and_navigation_buttons() {
        let (component, _recorded) = component_with_recorded_query(Some(("shift+enter", "enter")));
        let rendered = component.render(48);
        assert_eq!(rendered.len(), 3);
        for line in &rendered {
            assert_eq!(visible_width(line), 48, "line fits the width: {line:?}");
        }
        let top = &strip_terminal_sequences(&rendered[0]);
        assert!(top.starts_with('┌') && top.ends_with('┐') && top.contains("───"));
        assert_eq!(
            visible_width(top.trim_matches(|c| c == '┌' || c == '┐')),
            46
        );

        let middle_plain = strip_terminal_sequences(&rendered[1]);
        assert!(
            middle_plain.starts_with("│ Find in transcript "),
            "{middle_plain}"
        );
        assert!(middle_plain.ends_with("│"));
        assert!(rendered[1].contains("\x1b[2m"), "placeholder is muted");

        let bottom = strip_terminal_sequences(&rendered[2]);
        assert!(bottom.starts_with("└─"), "{bottom}");
        assert!(bottom.contains(" ↑ Shift+Enter · ↓ Enter "), "{bottom}");
        assert!(bottom.ends_with("─┘"), "{bottom}");

        // Hover targets come from the ranges render recorded.
        assert_eq!(
            component.get_navigation_direction_at(2, index_of(&rendered[2], "↑")),
            Some(-1)
        );
        assert_eq!(
            component.get_navigation_direction_at(2, index_of(&rendered[2], "Shift+Enter") + 5),
            Some(-1)
        );
        assert_eq!(
            component.get_navigation_direction_at(2, index_of(&rendered[2], "·")),
            None
        );
        assert_eq!(
            component.get_navigation_direction_at(2, index_of(&rendered[2], "↓")),
            Some(1)
        );
        assert_eq!(
            component.get_navigation_direction_at(2, last_index_of(&rendered[2], "Enter") + 2),
            Some(1)
        );
        assert_eq!(
            component.get_navigation_direction_at(0, index_of(&rendered[2], "↑")),
            None
        );
        assert_eq!(
            component.get_navigation_direction_at(1, index_of(&rendered[2], "↓")),
            None
        );
    }

    // B2.6 — typing then a result count swaps placeholder for the counter.
    #[test]
    fn typed_query_shows_right_aligned_muted_counter() {
        let (mut component, recorded) =
            component_with_recorded_query(Some(("shift+enter", "enter")));
        component.handle_input("n");
        component.set_result(0, 2);
        assert_eq!(recorded.lock().expect("lock").as_slice(), ["n"]);

        let rendered = component.render(48);
        assert_eq!(rendered.len(), 3);
        for line in &rendered {
            assert_eq!(visible_width(line), 48);
        }
        assert!(rendered[1].contains('n'));
        assert!(rendered[1].contains("1/2"));
        assert!(rendered[1].contains("\x1b[2m 1/2 \x1b[22m"));
        for line in &rendered {
            assert!(!line.contains("Find in transcript"));
        }

        component.set_result(-1, 0);
        let empty = component.render(48);
        assert!(strip_terminal_sequences(&empty[1]).contains("No matches"));
    }

    #[test]
    fn empty_query_renders_no_counter() {
        let (component, _recorded) = component_with_recorded_query(Some(("shift+enter", "enter")));
        let rendered = component.render(48);
        assert!(!rendered[1].contains("\x1b[2m 1/2"));
        assert_eq!(visible_width(&rendered[1]), 48);
    }

    // B2.7 — narrow widths drop the key labels; width 1 degenerates.
    #[test]
    fn narrow_width_falls_back_to_bare_arrows() {
        let (component, _recorded) = component_with_recorded_query(Some(("shift+enter", "enter")));
        let rendered = component.render(24);
        let bottom = strip_terminal_sequences(&rendered[2]);
        assert_eq!(bottom, format!("└{} ↑ ↓ {}┘", "─".repeat(16), "─"));
        assert_eq!(visible_width(&bottom), 24);
        assert_eq!(
            component.get_navigation_direction_at(2, index_of(&rendered[2], "↑")),
            Some(-1)
        );
        assert_eq!(
            component.get_navigation_direction_at(2, index_of(&rendered[2], "↓")),
            Some(1)
        );
        assert_eq!(
            component.get_navigation_direction_at(2, index_of(&rendered[2], "↑") + 2),
            Some(1)
        );
    }

    #[test]
    fn degenerate_width_renders_bare_borders() {
        let (component, _recorded) = component_with_recorded_query(None);
        assert_eq!(component.render(0), vec!["┌", "│", "└"]);
        let rendered = component.render(1);
        assert_eq!(rendered, vec!["┌", "│", "└"]);
        assert_eq!(component.navigation_button_ranges(), ((-1, -1), (-1, -1)));
    }

    #[test]
    fn narrow_enough_width_hides_buttons_entirely() {
        let (component, _recorded) = component_with_recorded_query(Some(("shift+enter", "enter")));
        let rendered = component.render(6);
        assert_eq!(rendered.len(), 3);
        for line in &rendered {
            assert_eq!(visible_width(line), 6);
        }
        let bottom = strip_terminal_sequences(&rendered[2]);
        assert!(!bottom.contains('↑') && !bottom.contains('↓'), "{bottom}");
        assert_eq!(component.navigation_button_ranges(), ((-1, -1), (-1, -1)));
        assert_eq!(component.get_navigation_direction_at(2, 3), None);
    }

    // B2.8 — hover change drives the re-render decision.
    #[test]
    fn hover_direction_reports_only_on_change() {
        let (mut component, _recorded) =
            component_with_recorded_query(Some(("shift+enter", "enter")));
        assert!(component.set_hovered_navigation_direction(Some(-1)));
        assert!(!component.set_hovered_navigation_direction(Some(-1)));
        assert!(component.set_hovered_navigation_direction(Some(1)));
        assert!(component.set_hovered_navigation_direction(None));
        assert_eq!(component.hovered_navigation_direction(), None);
    }

    // B2.8 — the button style hook receives the hovered flag.
    #[test]
    fn hovered_button_is_styled_by_the_caller_hook() {
        let mut options = AltScreenSearchComponentOptions::new(Arc::new(|_: &str| {}));
        options.previous_key = Some("shift+enter".to_string());
        options.next_key = Some("enter".to_string());
        options.navigation_button_style = Arc::new(|text: &str, hovered: bool| {
            if hovered {
                format!("\x1b[45m{text}\x1b[49m")
            } else {
                format!("\x1b[44m{text}\x1b[49m")
            }
        });
        let mut component = AltScreenSearchComponent::new(options);
        component.set_hovered_navigation_direction(Some(1));
        let rendered = component.render(48);
        assert!(rendered[2].contains("\x1b[45m↓ Enter\x1b[49m"));
        assert!(rendered[2].contains("\x1b[44m↑ Shift+Enter\x1b[49m"));
        assert_eq!(visible_width(&rendered[2]), 48);
    }

    #[test]
    fn input_is_consumed_and_focus_reaches_the_input() {
        let (mut component, _recorded) = component_with_recorded_query(None);
        component.set_focused(true);
        assert!(component.focused());
        assert!(component.has_input_handler());
        let mutation = component.handle_input("\x1b").expect("consumed");
        assert!(mutation.consume);
        component.invalidate();
        component.set_value("abc");
        assert_eq!(component.value(), "abc");
        component.set_value("abc");
        assert_eq!(component.value(), "abc");
    }

    #[test]
    fn unmatched_query_edit_does_not_notify() {
        let (mut component, recorded) = component_with_recorded_query(None);
        component.handle_input("a");
        // A bare escape is not a value change, so no second callback fires.
        component.handle_input("\x1b");
        assert_eq!(recorded.lock().expect("lock").len(), 1);
    }
}
