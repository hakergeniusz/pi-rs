//! Word navigation. `packages/tui/src/word-navigation.ts` (117 lines).
//!
//! `find_word_backward` / `find_word_forward` are ported rule for rule: skip
//! whitespace, then stop at the next word or punctuation boundary, honouring
//! the ASCII punctuation stops *inside* a word-like run.
//!
//! Three deviations, all forced by the port:
//!
//! * pi segments with `Intl.Segmenter("…", {granularity: "word"})`, which has
//!   no counterpart in this crate's dependency set. [`segment_words`] implements
//!   the UAX #29 word-boundary rules directly (`WB3`-`WB16`), which agrees with
//!   ICU on ASCII, Latin, Cyrillic, Greek, Hebrew and Katakana text — the corpora
//!   the editor and the alt-screen selection pin. Two families differ:
//!   `Intl.Segmenter` breaks Chinese and Japanese Kanji runs with a dictionary
//!   word breaker, where this port breaks between ideographs; and emoji, which
//!   ICU treats as one segment of `Other`, are likewise split here.
//! * Positions are **char indices**, not byte offsets and not pi's UTF-16 code
//!   units, because this port's editor addresses text by char index throughout.
//!   Callers slice with [`char_slice`].
//! * A segment's `chars` count is chars, while pi's is UTF-16 code units; the
//!   two differ only outside the BMP.

use unicode_width::UnicodeWidthChar;

/// `utils.ts` `PUNCTUATION_REGEX` = ``/[(){}[\]<>.,;:'"!?+\-=*/\\|&%^$#@~`]/``.
const PUNCTUATION: &[char] = &[
    '(', ')', '{', '}', '[', ']', '<', '>', '.', ',', ';', ':', '\'', '"', '!', '?', '+', '-', '=',
    '*', '/', '\\', '|', '&', '%', '^', '$', '#', '@', '~', '`',
];

/// Whether a character is one of pi's ASCII punctuation stops.
pub fn is_punctuation(c: char) -> bool {
    PUNCTUATION.contains(&c)
}

/// pi's `isWhitespaceChar`: `/\s/.test(char)`. `utils.ts:1011`.
pub fn is_whitespace_char(c: char) -> bool {
    c.is_whitespace()
}

/// The byte range of `text[start..end]` given **char** indices, or `None` when
/// either index is out of range or not on a char boundary.
pub fn char_slice(text: &str, start: usize, end: usize) -> Option<&str> {
    if end < start {
        return None;
    }
    let total = text.chars().count();
    if start > total || end > total {
        return None;
    }
    let start_byte = text
        .char_indices()
        .nth(start)
        .map_or(text.len(), |(index, _)| index);
    let end_byte = text
        .char_indices()
        .nth(end)
        .map_or(text.len(), |(index, _)| index);
    text.get(start_byte..end_byte)
}

/// One word-boundary segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSegment {
    /// The segment text.
    pub text: String,
    /// Its length in chars, which is what positions move by.
    pub chars: usize,
    /// pi's `Intl.SegmentData.isWordLike`.
    pub is_word_like: bool,
}

/// The UAX #29 word-boundary character classes this port needs. Only the
/// classes that change a decision are distinguished; everything unclassified is
/// `Other`, which is where every break happens (`WB999`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Other,
    ALetter,
    Numeric,
    Katakana,
    ExtendNumLet,
    Extend,
    RegionalIndicator,
    WSegSpace,
    Newline,
    CarriageReturn,
    LineFeed,
}

fn word_class(c: char) -> WordClass {
    match c {
        '\r' => return WordClass::CarriageReturn,
        '\n' => return WordClass::LineFeed,
        '\u{0b}' | '\u{0c}' | '\u{0085}' | '\u{2028}' | '\u{2029}' => return WordClass::Newline,
        '_' | '\u{203f}' | '\u{2040}' | '\u{2054}' | '\u{fe33}' | '\u{fe34}' | '\u{fe4d}'
        | '\u{fe4e}' | '\u{fe4f}' | '\u{ff3f}' => return WordClass::ExtendNumLet,
        _ => {}
    }
    let code = c as u32;
    if is_mark(c) || is_format(c) {
        return WordClass::Extend;
    }
    if (0x1f1e6..=0x1f1ff).contains(&code) {
        return WordClass::RegionalIndicator;
    }
    if c.is_whitespace() {
        return WordClass::WSegSpace;
    }
    if is_katakana(c) {
        return WordClass::Katakana;
    }
    if is_ideographic(c) {
        // `WordBreakProperty` excludes the ideographic scripts from `ALetter`,
        // which is why `Intl.Segmenter` keeps Han and kana apart from Latin.
        return WordClass::Other;
    }
    if c.is_alphabetic() {
        return WordClass::ALetter;
    }
    if c.is_numeric() {
        return WordClass::Numeric;
    }
    WordClass::Other
}

/// CJK ideographs and kana, `WordBreakProperty` `Ideographic` — plus hiragana,
/// which ICU also breaks character by character (`\u{3040}`-`\u{309f}`, minus the
/// two Katakana marks `\u{309b}`/`\u{309c}` that `is_katakana` claims first).
fn is_ideographic(c: char) -> bool {
    let code = c as u32;
    (0x3040..=0x309f).contains(&code)
        || (0x3005..=0x3007).contains(&code)
        || (0x3021..=0x3029).contains(&code)
        || (0x3038..=0x303a).contains(&code)
        || (0x3400..=0x4dbf).contains(&code)
        || (0x4e00..=0x9fff).contains(&code)
        || (0xf900..=0xfaff).contains(&code)
        || (0x20000..=0x3fffd).contains(&code)
}

/// `WordBreakProperty` `Katakana`, including the small kana and the halfwidth
/// block the terminal draws as Katakana.
fn is_katakana(c: char) -> bool {
    let code = c as u32;
    (0x3031..=0x3035).contains(&code)
        || (0x309b..=0x309c).contains(&code)
        || (0x30a0..=0x30ff).contains(&code)
        || (0x31f0..=0x31ff).contains(&code)
        || (0xff66..=0xff9d).contains(&code)
}

/// Whether a character counts towards `isWordLike` (ECMA-402: alphabetic,
/// numeric and mark).
fn counts_as_word_like(c: char) -> bool {
    c.is_alphanumeric() || (is_mark(c) && !is_format(c))
}

/// `MidLetter \u222a MidNumLetQ`, the characters that keep a word whole between
/// two letters (`WB6`, `WB7`). Derived from `WordBreakProperty.txt` as ICU
/// applies it; `.`, `'`, `\u{2018}`, `\u{2019}`, `\u{2024}` and `:` join
/// letters, while the fraction and thousands separators do not.
fn is_letter_mid(c: char) -> bool {
    matches!(
        c,
        '.' | ':'
            | '\''
            | '\u{00b7}'
            | '\u{0387}'
            | '\u{055f}'
            | '\u{05f4}'
            | '\u{2018}'
            | '\u{2019}'
            | '\u{2024}'
            | '\u{2027}'
            | '\u{fe13}'
            | '\u{fe52}'
            | '\u{fe55}'
            | '\u{ff07}'
            | '\u{ff0e}'
            | '\u{ff1a}'
    )
}

/// `MidNum \u222a MidNumLetQ`, the characters that keep a number whole
/// (`WB11`, `WB12`). Same source, different set: `:` does not join digits and
/// the fraction slash does, which is why `12:5` is three segments and `1,2`
/// is one.
fn is_digit_mid(c: char) -> bool {
    matches!(
        c,
        ',' | '.'
            | '\''
            | '\u{037e}'
            | '\u{0589}'
            | '\u{060c}'
            | '\u{060d}'
            | '\u{066c}'
            | '\u{07f8}'
            | '\u{2018}'
            | '\u{2019}'
            | '\u{2024}'
            | '\u{2044}'
            | '\u{fe50}'
            | '\u{fe52}'
            | '\u{fe54}'
            | '\u{ff07}'
            | '\u{ff0c}'
            | '\u{ff0e}'
    )
}

/// `Extended_Pictographic`, as far as the blocks that matter here: an emoji
/// joined by a zero-width joiner stays one segment (`WB13c`).
fn is_extended_pictographic(c: char) -> bool {
    let code = c as u32;
    matches!(
        code,
        0x00a9 | 0x00ae | 0x203c | 0x2049 | 0x2122 | 0x2139 | 0x3030 | 0x303d | 0x3297 | 0x3299
    ) || (0x2194..=0x21aa).contains(&code)
        || (0x2300..=0x23ff).contains(&code)
        || (0x24c2..=0x24ff).contains(&code)
        || (0x25a0..=0x27bf).contains(&code)
        || (0x2934..=0x2935).contains(&code)
        || (0x2b00..=0x2bff).contains(&code)
        || (0x1f000..=0x1faff).contains(&code)
}

/// `Format` characters, which `WB4` attaches like marks.
fn is_format(c: char) -> bool {
    matches!(c, '\u{00ad}' | '\u{200b}'..='\u{200f}' | '\u{2060}'..='\u{2064}' | '\u{feff}')
}

/// A combining mark, approximated by "takes no cell of its own". std has no
/// general-category query, and this port needs marks for exactly two
/// decisions: they attach to the character they follow (`WB4`) and they keep a
/// cluster word-like. Zero width identifies both — every mark in the corpora
/// pi pins is zero width, and the non-marks that are (ASCII controls, NUL,
/// ZWJ, variation selectors) are excluded for the same reasons.
fn is_mark(c: char) -> bool {
    if c.is_ascii() || c.is_whitespace() {
        return false;
    }
    UnicodeWidthChar::width(c) == Some(0)
}

/// `WordBreakProperty` `Pc`.
/// Segment `text` into word-boundary segments. See the module doc.
pub fn segment_words(text: &str) -> Vec<WordSegment> {
    let chars: Vec<char> = text.chars().collect();
    let classes: Vec<WordClass> = chars.iter().copied().map(word_class).collect();
    let mut segments: Vec<WordSegment> = Vec::new();
    let mut start = 0usize;
    for index in 1..chars.len() {
        if breaks_at(&chars, &classes, index) {
            push_segment(&mut segments, &chars[start..index]);
            start = index;
        }
    }
    if !chars.is_empty() {
        push_segment(&mut segments, &chars[start..]);
    }
    segments
}

fn push_segment(segments: &mut Vec<WordSegment>, chars: &[char]) {
    let text: String = chars.iter().collect();
    // ECMA-402: word-like is decided by whether the segment *contains* an
    // alphabetic, numeric or mark character, so `foo.bar` and `don't` are one
    // word-like segment while a lone `.` is not.
    let is_word_like = !chars.is_empty() && chars.iter().any(|c| counts_as_word_like(*c));
    segments.push(WordSegment {
        text,
        chars: chars.len(),
        is_word_like,
    });
}

/// Whether UAX #29 puts a boundary before `index`. `WB3`-`WB16`, then `WB999`.
///
/// `Extend` characters are transparent (`WB4`): they never end a segment and
/// every later rule reads the last significant character on their left.
fn breaks_at(chars: &[char], classes: &[WordClass], index: usize) -> bool {
    let before = index - 1;
    let raw_left = classes[before];
    let right = classes[index];
    if raw_left == WordClass::CarriageReturn && right == WordClass::LineFeed {
        return false; // WB3
    }
    if right == WordClass::Extend {
        return false; // WB4
    }
    if raw_left == WordClass::CarriageReturn || raw_left == WordClass::Newline {
        return true; // WB3a
    }
    if right == WordClass::CarriageReturn
        || right == WordClass::Newline
        || right == WordClass::LineFeed
    {
        return true; // WB3b
    }
    let Some(left_index) = previous_significant(classes, before) else {
        return true;
    };
    let left = classes[left_index];
    // The character *beyond* `right`, for the rules that read past a joiner.
    let after_right = next_significant(classes, index + 1).map(|at| classes[at]);
    let before_left =
        previous_significant(classes, left_index.saturating_sub(1)).map(|at| classes[at]);
    if left == WordClass::WSegSpace && right == WordClass::WSegSpace {
        return false; // WB3d
    }
    if left == WordClass::RegionalIndicator && right == WordClass::RegionalIndicator {
        return regional_indicator_run_length(classes, before) % 2 == 0; // WB15, WB16
    }
    if left == WordClass::ALetter && right == WordClass::ALetter {
        return false; // WB5
    }
    if left == WordClass::ALetter
        && is_letter_mid(chars[index])
        && after_right == Some(WordClass::ALetter)
    {
        return false; // WB6
    }
    if is_letter_mid(chars[left_index])
        && before_left == Some(WordClass::ALetter)
        && right == WordClass::ALetter
    {
        return false; // WB7
    }
    if left == WordClass::Numeric && right == WordClass::Numeric {
        return false; // WB8
    }
    if left == WordClass::ALetter && right == WordClass::Numeric {
        return false; // WB9
    }
    if left == WordClass::Numeric && right == WordClass::ALetter {
        return false; // WB10
    }
    if left == WordClass::Numeric
        && is_digit_mid(chars[index])
        && after_right == Some(WordClass::Numeric)
    {
        return false; // WB11
    }
    if is_digit_mid(chars[left_index])
        && before_left == Some(WordClass::Numeric)
        && right == WordClass::Numeric
    {
        return false; // WB12
    }
    if left == WordClass::Katakana && right == WordClass::Katakana {
        return false; // WB13
    }
    if is_extended_pictographic(chars[left_index])
        && is_extended_pictographic(chars[index])
        && only_joiners_between(chars, left_index, index)
    {
        return false; // WB13c
    }
    if matches!(
        left,
        WordClass::ALetter | WordClass::Numeric | WordClass::Katakana
    ) && right == WordClass::ExtendNumLet
    {
        return false; // WB13a
    }
    if left == WordClass::ExtendNumLet
        && matches!(
            right,
            WordClass::ALetter | WordClass::Numeric | WordClass::Katakana
        )
    {
        return false; // WB13b
    }
    true // WB999
}

/// Whether every character between two indexes is a zero-width joiner or a
/// mark, which is what makes `\u{1f468}\u{200d}\u{1f469}` one segment.
fn only_joiners_between(chars: &[char], from: usize, to: usize) -> bool {
    chars[from + 1..to]
        .iter()
        .all(|c| *c == '\u{200d}' || is_mark(*c) || is_format(*c))
}

/// How many regional indicators the run ending at `index` holds, skipping
/// `Extend`. `WB15` joins them in pairs.
fn regional_indicator_run_length(classes: &[WordClass], index: usize) -> usize {
    let mut count = 0usize;
    let mut at = Some(index);
    while let Some(position) = at {
        if classes[position] != WordClass::RegionalIndicator {
            break;
        }
        count += 1;
        at = previous_significant(classes, position.saturating_sub(1));
    }
    count
}

fn previous_significant(classes: &[WordClass], index: usize) -> Option<usize> {
    (0..=index)
        .rev()
        .find(|at| classes[*at] != WordClass::Extend)
}

fn next_significant(classes: &[WordClass], index: usize) -> Option<usize> {
    (index..classes.len()).find(|at| classes[*at] != WordClass::Extend)
}

/// How many chars of `segment` come after the last ASCII punctuation stop, or
/// the whole segment when it has none. pi's `word-navigation.ts:41-47`.
fn chars_after_last_punctuation_stop(segment: &WordSegment) -> usize {
    let chars: Vec<char> = segment.text.chars().collect();
    match chars.iter().rposition(|c| is_punctuation(*c)) {
        Some(index) => chars.len() - (index + 1),
        None => segment.chars,
    }
}

/// The char index of the first ASCII punctuation stop inside `segment`. pi:
/// `PUNCTUATION_REGEX.exec(segment)?.index` (`word-navigation.ts:109`).
fn first_punctuation_index(segment: &WordSegment) -> Option<usize> {
    segment.text.chars().position(is_punctuation)
}

/// The cursor position one word backward from `cursor`. `word-navigation.ts:17-72`.
///
/// `atomic` marks segments that must be skipped whole (the editor's paste
/// markers). `None` skips that feature, as pi does when no atomic segments
/// exist.
pub fn find_word_backward(
    text: &str,
    cursor: usize,
    atomic: Option<&dyn Fn(&str) -> bool>,
) -> usize {
    if cursor == 0 {
        return 0;
    }
    let before = char_slice(text, 0, cursor).unwrap_or(text);
    let mut segments = segment_words(before);
    let is_atomic = |segment: &WordSegment| -> bool {
        match atomic {
            Some(predicate) => predicate(&segment.text),
            None => false,
        }
    };
    let mut new_cursor = cursor;

    // Skip trailing whitespace.
    while let Some(last) = segments.last() {
        if is_atomic(last) || !is_whitespace_char(last.text.chars().next().unwrap_or(' ')) {
            break;
        }
        new_cursor -= last.chars;
        segments.pop();
    }

    let Some(last) = segments.pop() else {
        return new_cursor;
    };

    if is_atomic(&last) {
        new_cursor -= last.chars;
    } else if last.is_word_like {
        new_cursor -= chars_after_last_punctuation_stop(&last);
    } else {
        // Punctuation run: skip the whole run, whitespace included.
        new_cursor -= last.chars;
        while let Some(previous) = segments.last() {
            if is_atomic(previous) || previous.is_word_like {
                break;
            }
            new_cursor -= previous.chars;
            segments.pop();
        }
    }
    new_cursor
}

/// The cursor position one word forward from `cursor`. `word-navigation.ts:75-117`.
pub fn find_word_forward(
    text: &str,
    cursor: usize,
    atomic: Option<&dyn Fn(&str) -> bool>,
) -> usize {
    let total = text.chars().count();
    if cursor >= total {
        return total;
    }
    let after = char_slice(text, cursor, total).unwrap_or(text);
    let segments = segment_words(after);
    let is_atomic = |segment: &WordSegment| -> bool {
        match atomic {
            Some(predicate) => predicate(&segment.text),
            None => false,
        }
    };
    let mut new_cursor = cursor;
    let mut iter = segments.iter().peekable();

    // Skip leading whitespace.
    while let Some(segment) = iter.peek() {
        if is_atomic(segment) || !is_whitespace_char(segment.text.chars().next().unwrap_or(' ')) {
            break;
        }
        new_cursor += segment.chars;
        iter.next();
    }

    let Some(first) = iter.next() else {
        return new_cursor;
    };

    if is_atomic(first) {
        new_cursor += first.chars;
    } else if first.is_word_like {
        new_cursor += first_punctuation_index(first).unwrap_or(first.chars);
    } else {
        // Punctuation run: stop at the end of the run.
        new_cursor += first.chars;
        for segment in iter {
            if is_atomic(segment) || segment.is_word_like {
                break;
            }
            new_cursor += segment.chars;
        }
    }
    new_cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(segments: &[WordSegment]) -> Vec<&str> {
        segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect()
    }

    #[test]
    fn segments_split_whitespace_punctuation_and_words() {
        let segments = segment_words("foo, bar_baz");
        assert_eq!(texts(&segments), ["foo", ",", " ", "bar_baz"]);
        assert!(segments[0].is_word_like);
        assert!(!segments[1].is_word_like, "punctuation is not word-like");
    }

    #[test]
    fn char_slice_uses_char_indices() {
        assert_eq!(char_slice("héllo", 0, 2), Some("hé"));
        assert_eq!(char_slice("héllo", 1, 5), Some("éllo"));
        assert_eq!(char_slice("héllo", 0, 9), None);
    }

    #[test]
    fn backward_at_zero_stays_at_zero() {
        assert_eq!(find_word_backward("abc", 0, None), 0);
    }

    #[test]
    fn backward_skips_whitespace_then_one_word() {
        assert_eq!(find_word_backward("foo bar", 7, None), 4);
        assert_eq!(find_word_backward("foo bar", 6, None), 4);
    }

    #[test]
    fn backward_walks_two_words() {
        assert_eq!(find_word_backward("foo bar baz", 11, None), 8);
    }

    #[test]
    fn forward_skips_whitespace_then_the_whole_word_run() {
        // From the space before "bar": skip it, then the whole word-like run.
        assert_eq!(find_word_forward("foo bar", 3, None), 7);
    }

    #[test]
    fn forward_stops_at_the_punctuation_inside_a_word() {
        assert_eq!(find_word_forward("foo.bar", 0, None), 3);
    }

    #[test]
    fn forward_at_the_end_stays_at_the_end() {
        assert_eq!(find_word_forward("foo", 3, None), 3);
    }

    #[test]
    fn punctuation_runs_are_one_move() {
        assert_eq!(find_word_forward("a...b", 1, None), 4);
        assert_eq!(find_word_backward("a...b", 5, None), 4);
    }

    #[test]
    fn ascii_punctuation_boundaries_inside_a_word_run_stop_the_move() {
        // `findWordBackward` keeps the punctuation with the text before it.
        assert_eq!(find_word_backward("foo.bar", 7, None), 4);
    }

    #[test]
    fn astral_characters_count_as_one_char() {
        assert_eq!(find_word_forward("a→b", 1, None), 2);
        assert_eq!(find_word_backward("a→b", 3, None), 2);
    }

    #[test]
    fn an_atomic_segment_is_skipped_whole() {
        let marker = |text: &str| text == "«";
        assert_eq!(find_word_backward("a«b", 2, Some(&marker)), 1);
        assert_eq!(find_word_forward("a«b", 1, Some(&marker)), 2);
    }

    #[test]
    fn without_an_atomic_predicate_the_run_is_skipped_whole() {
        assert_eq!(find_word_backward("a«b", 2, None), 1);
        assert_eq!(find_word_forward("a«b", 1, None), 2);
    }
}
