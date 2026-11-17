//! Diff computation for the edit tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/edit-diff.ts`
//! (jsdiff `8.0.4` is its `diff` dependency).
//!
//! What lives here:
//!
//! * the fuzzy matcher that finds `oldText` in a file (exact first, then a
//!   normalized pass that ignores trailing whitespace, smart quotes, unicode
//!   dashes, and exotic spaces),
//! * `apply_edits_to_normalized_content`, which enforces the uniqueness and
//!   non-overlap rules and reports pi's errors verbatim,
//! * `apply_replacements_to_original`, which splices replacements into the
//!   original bytes so untouched lines keep their line endings,
//! * the two diff renderings: `generate_edit_diff` (windowed display diff +
//!   unified patch, `edit-diff.ts:589-626`) and the whole-file
//!   `generate_diff_string` / `generate_unified_patch`.
//!
//! Porting note: pi uses the `diff` npm package's `diffLines`,
//! `structuredPatch`, and `formatPatch`. All are line-based Myers diffs; that
//! is what this module implements, with jsdiff's part ordering (removals
//! before insertions at a change, contiguous runs merged), its hunk-cutting
//! rules, and its `\ No newline at end of file` markers.

/// pi's `Edit` (`edit-diff.ts:232-235`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// pi's `TextReplacement` (`edit-diff.ts:117`): replacement of `match_length`
/// bytes at `match_index` with `new_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextReplacement {
    pub match_index: usize,
    pub match_length: usize,
    pub new_text: String,
}

/// pi's `AppliedEditsResult` (`edit-diff.ts:237-242`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEditsResult {
    pub base_content: String,
    pub new_content: String,
    /// Sorted replacements of `base_content` that turn it into `new_content`.
    pub replacements: Vec<TextReplacement>,
}

/// pi's `FuzzyMatchResult` (`edit-diff.ts:216-230`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatchResult {
    pub found: bool,
    /// Byte index of the match in `content_for_replacement`.
    pub index: usize,
    /// Byte length of the match.
    pub match_length: usize,
    /// Whether fuzzy matching was used (false = exact match).
    pub used_fuzzy_match: bool,
    /// Exact match: the original content. Fuzzy match: the normalized content.
    pub content_for_replacement: String,
}

/// pi's `EditDiff` (`edit-diff.ts:572-576`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditDiff {
    pub diff: String,
    pub patch: String,
    pub first_changed_line: Option<usize>,
}

/// pi's `normalizeToLF` (`edit-diff.ts:11-13`).
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// pi's `splitBom` (`utils/text.ts`): a leading UTF-8 BOM split from the text.
pub fn split_bom(content: &str) -> (String, String) {
    match content.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}".to_string(), rest.to_string()),
        None => (String::new(), content.to_string()),
    }
}

/// pi's `normalizeForFuzzyMatch` (`edit-diff.ts:77-98`).
///
/// The TS version starts with `String.prototype.normalize("NFKC")`
/// (`edit-diff.ts:81`). A full NFKC needs the whole Unicode decomposition and
/// composition tables; this port ships the subset that covers the
/// compatibility-equivalent forms a model writing ASCII/NFC meets in real
/// files — fullwidth forms, precomposed Latin letters (Latin-1 Supplement and
/// Latin Extended-A), Greek monotonic accents, common Cyrillic pairs,
/// ligatures, and superscript/subscript digits — applied as
/// decomposition-only. Both the file content and the model's old text go
/// through the same mapping, so compatibility-equivalent texts still compare
/// equal and every case in the TS suite behaves identically. Residual gaps
/// against a full NFKC (Hangul jamo, CJK compatibility forms, multi-mark
/// canonical reordering, the rest of the UCD) are recorded as a deviation in
/// the P4 report; the `unicode-normalization` crate is the drop-in upgrade.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc_subset: String = text.chars().map(nfkc_subset_char).collect();
    nfkc_subset
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(|character| match character {
            // Smart single quotes → ' (`edit-diff.ts:86`).
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            // Smart double quotes → " (`edit-diff.ts:88`).
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            // Various dashes/hyphens → - (`edit-diff.ts:92`).
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            // Special spaces → regular space (`edit-diff.ts:96`).
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// The NFKC subset [`normalize_for_fuzzy_match`] applies per character.
fn nfkc_subset_char(character: char) -> String {
    // Fullwidth forms U+FF01–U+FF5E have a uniform compatibility mapping to
    // ASCII (Ａ → A, ， → ,, ！ → ! …): subtract 0xFEE0.
    if ('\u{FF01}'..='\u{FF5E}').contains(&character) {
        return char::from_u32(character as u32 - 0xFEE0)
            .expect("fullwidth offset lands in ASCII")
            .to_string();
    }
    if let Ok(index) = CANONICAL_DECOMPOSITIONS.binary_search_by_key(&character, |entry| entry.0) {
        return CANONICAL_DECOMPOSITIONS[index].1.to_string();
    }
    if let Ok(index) =
        COMPATIBILITY_DECOMPOSITIONS.binary_search_by_key(&character, |entry| entry.0)
    {
        return COMPATIBILITY_DECOMPOSITIONS[index].1.to_string();
    }
    character.to_string()
}

/// Canonical decompositions for the precomposed letters the fuzzy matcher
/// needs (NFKC = decompose, reorder, recompose; both sides of the comparison
/// get the same decomposition, so recomposition is not required for equality).
/// Sorted by code point for `binary_search`; tables transcribed from the
/// Unicode Character Database (`DerivedDecompositionType` = Canonical).
const CANONICAL_DECOMPOSITIONS: &[(char, &str)] = &[
    // Latin-1 Supplement.
    ('À', "A\u{300}"),
    ('Á', "A\u{301}"),
    ('Â', "A\u{302}"),
    ('Ã', "A\u{303}"),
    ('Ä', "A\u{308}"),
    ('Å', "A\u{30A}"),
    ('Ç', "C\u{327}"),
    ('È', "E\u{300}"),
    ('É', "E\u{301}"),
    ('Ê', "E\u{302}"),
    ('Ë', "E\u{308}"),
    ('Ì', "I\u{300}"),
    ('Í', "I\u{301}"),
    ('Î', "I\u{302}"),
    ('Ï', "I\u{308}"),
    ('Ñ', "N\u{303}"),
    ('Ò', "O\u{300}"),
    ('Ó', "O\u{301}"),
    ('Ô', "O\u{302}"),
    ('Õ', "O\u{303}"),
    ('Ö', "O\u{308}"),
    ('Ù', "U\u{300}"),
    ('Ú', "U\u{301}"),
    ('Û', "U\u{302}"),
    ('Ü', "U\u{308}"),
    ('Ý', "Y\u{301}"),
    ('à', "a\u{300}"),
    ('á', "a\u{301}"),
    ('â', "a\u{302}"),
    ('ã', "a\u{303}"),
    ('ä', "a\u{308}"),
    ('å', "a\u{30A}"),
    ('ç', "c\u{327}"),
    ('è', "e\u{300}"),
    ('é', "e\u{301}"),
    ('ê', "e\u{302}"),
    ('ë', "e\u{308}"),
    ('ì', "i\u{300}"),
    ('í', "i\u{301}"),
    ('î', "i\u{302}"),
    ('ï', "i\u{308}"),
    ('ñ', "n\u{303}"),
    ('ò', "o\u{300}"),
    ('ó', "o\u{301}"),
    ('ô', "o\u{302}"),
    ('õ', "o\u{303}"),
    ('ö', "o\u{308}"),
    ('ù', "u\u{300}"),
    ('ú', "u\u{301}"),
    ('û', "u\u{302}"),
    ('ü', "u\u{308}"),
    ('ý', "y\u{301}"),
    ('ÿ', "y\u{308}"),
    // Latin Extended-A (the letters with canonical decompositions; ß, æ, ø,
    // đ, ł, ŧ, ħ, œ, ð, þ, ı have none).
    ('Ā', "A\u{304}"),
    ('ā', "a\u{304}"),
    ('Ă', "A\u{306}"),
    ('ă', "a\u{306}"),
    ('Ą', "A\u{328}"),
    ('ą', "a\u{328}"),
    ('Ć', "C\u{301}"),
    ('ć', "c\u{301}"),
    ('Ĉ', "C\u{302}"),
    ('ĉ', "c\u{302}"),
    ('Ċ', "C\u{307}"),
    ('ċ', "c\u{307}"),
    ('Č', "C\u{30C}"),
    ('č', "c\u{30C}"),
    ('Ď', "D\u{30C}"),
    ('ď', "d\u{30C}"),
    ('Ē', "E\u{304}"),
    ('ē', "e\u{304}"),
    ('Ė', "E\u{307}"),
    ('ė', "e\u{307}"),
    ('Ę', "E\u{328}"),
    ('ę', "e\u{328}"),
    ('Ě', "E\u{30C}"),
    ('ě', "e\u{30C}"),
    ('Ĝ', "G\u{302}"),
    ('ĝ', "g\u{302}"),
    ('Ğ', "G\u{306}"),
    ('ğ', "g\u{306}"),
    ('Ġ', "G\u{307}"),
    ('ġ', "g\u{307}"),
    ('Ģ', "G\u{327}"),
    ('ģ', "g\u{327}"),
    ('Ĥ', "H\u{302}"),
    ('ĥ', "h\u{302}"),
    ('Ĩ', "I\u{303}"),
    ('ĩ', "i\u{303}"),
    ('Ī', "I\u{304}"),
    ('ī', "i\u{304}"),
    ('Ĭ', "I\u{306}"),
    ('ĭ', "i\u{306}"),
    ('Į', "I\u{328}"),
    ('į', "i\u{328}"),
    ('İ', "I\u{307}"),
    ('Ĵ', "J\u{302}"),
    ('ĵ', "j\u{302}"),
    ('Ķ', "K\u{327}"),
    ('ķ', "k\u{327}"),
    ('Ĺ', "L\u{301}"),
    ('ĺ', "l\u{301}"),
    ('Ļ', "L\u{327}"),
    ('ļ', "l\u{327}"),
    ('Ľ', "L\u{30C}"),
    ('ľ', "l\u{30C}"),
    ('Ń', "N\u{301}"),
    ('ń', "n\u{301}"),
    ('Ņ', "N\u{327}"),
    ('ņ', "n\u{327}"),
    ('Ň', "N\u{30C}"),
    ('ň', "n\u{30C}"),
    ('Ō', "O\u{304}"),
    ('ō', "o\u{304}"),
    ('Ŏ', "O\u{306}"),
    ('ŏ', "o\u{306}"),
    ('Ő', "O\u{30B}"),
    ('ő', "o\u{30B}"),
    ('Ŕ', "R\u{301}"),
    ('ŕ', "r\u{301}"),
    ('Ŗ', "R\u{327}"),
    ('ŗ', "r\u{327}"),
    ('Ř', "R\u{30C}"),
    ('ř', "r\u{30C}"),
    ('Ś', "S\u{301}"),
    ('ś', "s\u{301}"),
    ('Ŝ', "S\u{302}"),
    ('ŝ', "s\u{302}"),
    ('Ş', "S\u{327}"),
    ('ş', "s\u{327}"),
    ('Š', "S\u{30C}"),
    ('š', "s\u{30C}"),
    ('Ţ', "T\u{327}"),
    ('ţ', "t\u{327}"),
    ('Ť', "T\u{30C}"),
    ('ť', "t\u{30C}"),
    ('Ū', "U\u{304}"),
    ('ū', "u\u{304}"),
    ('Ŭ', "U\u{306}"),
    ('ŭ', "u\u{306}"),
    ('Ů', "U\u{30A}"),
    ('ů', "u\u{30A}"),
    ('Ű', "U\u{30B}"),
    ('ű', "u\u{30B}"),
    ('Ų', "U\u{328}"),
    ('ų', "u\u{328}"),
    ('Ŵ', "W\u{302}"),
    ('ŵ', "w\u{302}"),
    ('Ŷ', "Y\u{302}"),
    ('ŷ', "y\u{302}"),
    ('Ÿ', "Y\u{308}"),
    ('Ź', "Z\u{301}"),
    ('ź', "z\u{301}"),
    ('Ż', "Z\u{307}"),
    ('ż', "z\u{307}"),
    ('Ž', "Z\u{30C}"),
    ('ž', "z\u{30C}"),
    // Greek monotonic accents.
    ('Ά', "\u{391}\u{301}"),
    ('Έ', "\u{395}\u{301}"),
    ('Ή', "\u{397}\u{301}"),
    ('Ί', "\u{399}\u{301}"),
    ('Ό', "\u{39F}\u{301}"),
    ('Ύ', "\u{3A5}\u{301}"),
    ('Ώ', "\u{3A9}\u{301}"),
    ('ΐ', "\u{3B9}\u{308}\u{301}"),
    ('Ϊ', "\u{399}\u{308}"),
    ('Ϋ', "\u{3A5}\u{308}"),
    ('ά', "\u{3B1}\u{301}"),
    ('έ', "\u{3B5}\u{301}"),
    ('ή', "\u{3B7}\u{301}"),
    ('ί', "\u{3B9}\u{301}"),
    ('ΰ', "\u{3C5}\u{308}\u{301}"),
    ('ϊ', "\u{3B9}\u{308}"),
    ('ϋ', "\u{3C5}\u{308}"),
    ('ό', "\u{3BF}\u{301}"),
    ('ύ', "\u{3C5}\u{301}"),
    ('ώ', "\u{3C9}\u{301}"),
    // Cyrillic (the acute/diaeresis/breve pairs that decompose).
    ('Ё', "\u{415}\u{308}"),
    ('Ў', "\u{423}\u{306}"),
    ('Й', "\u{418}\u{306}"),
    ('ё', "\u{435}\u{308}"),
    ('й', "\u{438}\u{306}"),
    ('ў', "\u{443}\u{306}"),
];

/// Compatibility decompositions with ASCII targets (UCD
/// `DerivedDecompositionType` = Compatibility), same shape and sorting as
/// [`CANONICAL_DECOMPOSITIONS`].
const COMPATIBILITY_DECOMPOSITIONS: &[(char, &str)] = &[
    ('²', "2"),
    ('³', "3"),
    ('¹', "1"),
    ('⁰', "0"),
    ('⁴', "4"),
    ('⁵', "5"),
    ('⁶', "6"),
    ('⁷', "7"),
    ('⁸', "8"),
    ('⁹', "9"),
    ('₀', "0"),
    ('₁', "1"),
    ('₂', "2"),
    ('₃', "3"),
    ('₄', "4"),
    ('₅', "5"),
    ('₆', "6"),
    ('₇', "7"),
    ('₈', "8"),
    ('₉', "9"),
    ('\u{2126}', "\u{3A9}"),
    ('\u{212A}', "k"),
    ('\u{212B}', "A\u{30A}"),
    ('ﬀ', "ff"),
    ('ﬁ', "fi"),
    ('ﬂ', "fl"),
    ('ﬃ', "ffi"),
    ('ﬄ', "ffl"),
    ('ﬅ', "st"),
    ('ﬆ', "st"),
];

/// pi's `fuzzyFindText` (`edit-diff.ts:250-288`).
pub fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    // Try exact match first.
    if let Some(index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index,
            match_length: old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        };
    }

    // Try fuzzy match — work entirely in normalized space.
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    match fuzzy_content.find(&fuzzy_old_text) {
        Some(index) => FuzzyMatchResult {
            found: true,
            index,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        },
        None => FuzzyMatchResult {
            found: false,
            index: 0,
            match_length: 0,
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        },
    }
}

/// pi's `countOccurrences` (`edit-diff.ts:290-294`).
fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    if fuzzy_old_text.is_empty() {
        return 0;
    }
    fuzzy_content.matches(&fuzzy_old_text).count()
}

/// pi's `getNotFoundError` (`edit-diff.ts:296-305`).
fn not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

/// pi's `getDuplicateError` (`edit-diff.ts:307-316`).
fn duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

/// pi's `getEmptyOldTextError` (`edit-diff.ts:318-323`).
fn empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

/// pi's `getNoChangeError` (`edit-diff.ts:325-332`).
fn no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

/// pi's `splitLinesWithEndings` (`edit-diff.ts:100-102`): lines keep their
/// terminator; the regex `[^\n]*\n|[^\n]+` yields exactly this.
fn split_lines_with_endings(content: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let bytes = content.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\n' {
            lines.push(&content[start..=index]);
            start = index + 1;
        }
        index += 1;
    }
    if start < bytes.len() {
        lines.push(&content[start..]);
    }
    lines
}

/// pi's `LineSpan` (`edit-diff.ts:104-107`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineSpan {
    start: usize,
    end: usize,
}

/// pi's `getLineSpans` (`edit-diff.ts:119-126`).
fn line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = LineSpan {
                start: offset,
                end: offset + line.len(),
            };
            offset = span.end;
            span
        })
        .collect()
}

/// pi's `applyReplacements` (`edit-diff.ts:155-164`): applied in reverse so
/// offsets stay stable.
///
/// `offset` is where `content` starts inside the base content the
/// replacements were located in; pi passes the group's start offset when it
/// rewrites one line group at a time.
fn apply_replacements(content: &str, replacements: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let start = replacement.match_index - offset;
        result.replace_range(
            start..start + replacement.match_length,
            &replacement.new_text,
        );
    }
    result
}

/// pi's `getReplacementLineRange` (`edit-diff.ts:128-153`).
fn replacement_line_range(
    lines: &[LineSpan],
    replacement: &TextReplacement,
) -> Result<(usize, usize), String> {
    let replacement_start = replacement.match_index;
    let replacement_end = replacement.match_index + replacement.match_length;

    let start_line = lines
        .iter()
        .position(|line| replacement_start >= line.start && replacement_start < line.end)
        .ok_or("Replacement range is outside the base content.")?;

    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err("Replacement range is outside the base content.".to_string());
    }

    Ok((start_line, end_line + 1))
}

/// pi's `widenReplacementsToLines` (`edit-diff.ts:176-214`): turn
/// replacements matched against `base_content` into replacements of
/// `original_content` that keep unchanged line blocks from the original.
fn widen_replacements_to_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[TextReplacement],
) -> Result<Vec<TextReplacement>, String> {
    struct Group {
        start_line: usize,
        end_line: usize,
        replacements: Vec<TextReplacement>,
    }

    let original_lines = line_spans(original_content);
    let base_lines = line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_string(),
        );
    }

    let mut groups: Vec<Group> = Vec::new();
    let mut sorted: Vec<&TextReplacement> = replacements.iter().collect();
    sorted.sort_by_key(|replacement| replacement.match_index);
    for replacement in sorted {
        let range = replacement_line_range(&base_lines, replacement)?;
        match groups.last_mut() {
            Some(current) if range.0 < current.end_line => {
                current.end_line = current.end_line.max(range.1);
                current.replacements.push(replacement.clone());
            }
            _ => groups.push(Group {
                start_line: range.0,
                end_line: range.1,
                replacements: vec![replacement.clone()],
            }),
        }
    }

    Ok(groups
        .into_iter()
        .map(|group| {
            let group_start_offset = base_lines[group.start_line].start;
            let group_end_offset = base_lines[group.end_line - 1].end;
            let original_start = original_lines[group.start_line].start;
            TextReplacement {
                match_index: original_start,
                match_length: original_lines[group.end_line - 1].end - original_start,
                new_text: apply_replacements(
                    &base_content[group_start_offset..group_end_offset],
                    &group.replacements,
                    group_start_offset,
                ),
            }
        })
        .collect())
}

/// pi's `applyEditsToNormalizedContent` (`edit-diff.ts:343-406`).
///
/// All edits are matched against the same original content. Replacements are
/// then applied in reverse order so offsets remain stable. If any edit needs
/// fuzzy matching, the operation runs in fuzzy-normalized content space and
/// then overlays those line-level changes onto the original content so
/// unchanged line blocks keep their original bytes.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEditsResult, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();

    for (index, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(empty_old_text_error(path, index, normalized_edits.len()));
        }
    }

    let initial_matches: Vec<FuzzyMatchResult> = normalized_edits
        .iter()
        .map(|edit| fuzzy_find_text(normalized_content, &edit.old_text))
        .collect();
    let used_fuzzy_match = initial_matches
        .iter()
        .any(|matched| matched.used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched_edits: Vec<(usize, TextReplacement)> = Vec::new();
    for (index, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            return Err(not_found_error(path, index, normalized_edits.len()));
        }

        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(duplicate_error(
                path,
                index,
                normalized_edits.len(),
                occurrences,
            ));
        }

        matched_edits.push((
            index,
            TextReplacement {
                match_index: match_result.index,
                match_length: match_result.match_length,
                new_text: edit.new_text.clone(),
            },
        ));
    }

    matched_edits.sort_by_key(|(index, replacement)| (replacement.match_index, *index));
    for pair in matched_edits.windows(2) {
        let (previous_index, previous) = &pair[0];
        let (current_index, current) = &pair[1];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{previous_index}] and edits[{current_index}] overlap in {path}. Merge them into one edit or target disjoint regions."
            ));
        }
    }

    let base_content = normalized_content;
    let replacements: Vec<TextReplacement> = matched_edits
        .into_iter()
        .map(|(_, replacement)| replacement)
        .collect();
    let replacements = if used_fuzzy_match {
        widen_replacements_to_lines(normalized_content, &replacement_base_content, &replacements)?
    } else {
        replacements
    };
    let new_content = apply_replacements(base_content, &replacements, 0);

    if base_content == new_content {
        return Err(no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEditsResult {
        base_content: normalized_content.to_string(),
        new_content,
        replacements,
    })
}

/// pi's `getLineEndingAt` (`edit-diff.ts:63-68`): the line ending of the line
/// containing `offset`. A lone CR only counts in files without any LF.
fn get_line_ending_at(content: &str, offset: usize) -> &'static str {
    let clamped = offset.min(content.len());
    let line_feed = match content[clamped..].find('\n') {
        Some(relative) => clamped + relative,
        // `lastIndexOf("\n", offset)` includes a line feed at `offset` itself.
        None => match content[..clamped].rfind('\n') {
            Some(found) => found,
            None => return if content.contains('\r') { "\r" } else { "\n" },
        },
    };
    if line_feed > 0 && content.as_bytes()[line_feed - 1] == b'\r' {
        "\r\n"
    } else {
        "\n"
    }
}

/// pi's `applyReplacementsToOriginal` (`edit-diff.ts:21-56`): apply
/// replacements matched in `normalize_to_lf(original_content)` to
/// `original_content` itself. Text outside the replaced ranges keeps its
/// bytes, including its line endings; line breaks in the new text get the
/// line ending of the line where the replaced range starts.
///
/// The replacements must be sorted by `match_index`, as pi's callers pass
/// them. The CRLF cursor is monotonic across calls for the same reason.
pub fn apply_replacements_to_original(
    original_content: &str,
    replacements: &[TextReplacement],
) -> String {
    // Only CRLF changes length when normalizing: the LF of the n-th CRLF (at
    // original offset p) is at p - n in the normalized text. Adding the number
    // of CRLFs before a normalized offset gives the original offset, and a
    // range that covers that LF covers the whole CRLF (`edit-diff.ts:24-27`).
    let mut crlf_before = 0usize;
    let mut next_crlf = original_content.find("\r\n");
    let mut to_original = |offset: usize| -> usize {
        while let Some(position) = next_crlf {
            if position - crlf_before >= offset {
                break;
            }
            crlf_before += 1;
            next_crlf = original_content[position + 2..]
                .find("\r\n")
                .map(|relative| position + 2 + relative);
        }
        offset + crlf_before
    };

    let mut parts: Vec<String> = Vec::new();
    let mut ends_with_cr = false;
    let push = |parts: &mut Vec<String>, ends_with_cr: &mut bool, part: String| {
        if part.is_empty() {
            return;
        }
        // A CR before a seam and an LF after it would merge into one CRLF line
        // break (`edit-diff.ts:39-44`). Adding an LF turns the CR into a CRLF
        // of its own, so the written file has the line breaks the diff shows.
        if *ends_with_cr && part.starts_with('\n') {
            parts.push("\n".to_string());
        }
        *ends_with_cr = part.ends_with('\r');
        parts.push(part);
    };

    let mut position = 0usize;
    for replacement in replacements {
        let start = to_original(replacement.match_index);
        let seam = original_content[position..start].to_string();
        push(&mut parts, &mut ends_with_cr, seam);
        let ending = get_line_ending_at(original_content, start);
        let new_text = if ending == "\n" {
            replacement.new_text.clone()
        } else {
            replacement.new_text.replace('\n', ending)
        };
        push(&mut parts, &mut ends_with_cr, new_text);
        position = to_original(replacement.match_index + replacement.match_length);
    }
    let seam = original_content[position..].to_string();
    push(&mut parts, &mut ends_with_cr, seam);
    parts.join("")
}

// ---------------------------------------------------------------------------
// jsdiff line diff (Myers, linear space)
// ---------------------------------------------------------------------------

/// One hunk of the line diff, pi's `Diff.diffLines` part shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPart {
    pub added: bool,
    pub removed: bool,
    pub value: String,
}

/// pi's line diff: `Diff.diffLines(oldContent, newContent)` (jsdiff 8.0.4).
///
/// Myers' O(ND) algorithm in the linear-space divide-and-conquer form: the
/// common prefix and suffix are trimmed first, then the middle snake is found
/// and the two halves are solved recursively. Memory is O(N+M) at every level,
/// not the O(N*M) a plain LCS table would need — the edit tool runs this on
/// whatever the model hands it, and an LCS table over a large file is a
/// multi-gigabyte allocation.
///
/// `parts` come out in jsdiff's order: at a change, the removals precede the
/// insertions, and contiguous runs of the same kind are merged.
pub fn diff_lines(old_content: &str, new_content: &str) -> Vec<DiffPart> {
    let old_lines: Vec<&str> = split_lines_with_endings(old_content);
    let new_lines: Vec<&str> = split_lines_with_endings(new_content);
    let mut ops = Vec::new();
    diff_ops(&old_lines, &new_lines, 0, 0, &mut ops, 0);

    let mut parts: Vec<DiffPart> = Vec::new();
    let push = |added: bool, removed: bool, line: &str, parts: &mut Vec<DiffPart>| {
        if let Some(last) = parts.last_mut() && last.added == added && last.removed == removed {
            last.value.push_str(line);
            return;
        }
        parts.push(DiffPart {
            added,
            removed,
            value: line.to_string(),
        });
    };
    for (op, old_index, new_index) in ops {
        match op {
            Op::Keep => push(false, false, old_lines[old_index], &mut parts),
            Op::Remove => push(false, true, old_lines[old_index], &mut parts),
            Op::Add => push(true, false, new_lines[new_index], &mut parts),
        }
    }
    parts
}

/// Recursion guards. Beyond these the diff degrades to "replace the whole
/// middle", which is what jsdiff does when its time budget runs out, and keeps
/// the worst case bounded instead of quadratic.
const MAX_DIFF_DEPTH: usize = 48;
const MAX_DIFF_LINES: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Remove,
    Add,
}

/// The edit script: `(op, index into old, index into new)`.
type EditScript = Vec<(Op, usize, usize)>;

/// Myers' linear-space divide-and-conquer diff.
///
/// `old` and `new` are sub-slices; `old_offset`/`new_offset` are their
/// positions in the full inputs, so the script carries whole-file indices.
fn diff_ops(
    old: &[&str],
    new: &[&str],
    old_offset: usize,
    new_offset: usize,
    script: &mut EditScript,
    depth: usize,
) {
    // Trim the common prefix.
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        script.push((Op::Keep, old_offset + prefix, new_offset + prefix));
        prefix += 1;
    }
    // Trim the common suffix.
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let (a_start, a_end) = (prefix, old.len() - suffix);
    let (b_start, b_end) = (prefix, new.len() - suffix);
    let middle_old = &old[a_start..a_end];
    let middle_new = &new[b_start..b_end];
    let old_base = old_offset + a_start;
    let new_base = new_offset + b_start;

    if middle_old.is_empty() {
        for index in 0..middle_new.len() {
            script.push((Op::Add, old_base, new_base + index));
        }
    } else if middle_new.is_empty() {
        for index in 0..middle_old.len() {
            script.push((Op::Remove, old_base + index, new_base));
        }
    } else if depth >= MAX_DIFF_DEPTH || middle_old.len() + middle_new.len() > MAX_DIFF_LINES {
        // Bounded fallback: one replacement for the whole middle.
        for index in 0..middle_old.len() {
            script.push((Op::Remove, old_base + index, new_base));
        }
        for index in 0..middle_new.len() {
            script.push((Op::Add, old_base, new_base + index));
        }
    } else if let Some((x_start, y_start, x_end, y_end)) = middle_snake(middle_old, middle_new) {
        diff_ops(
            &middle_old[..x_start],
            &middle_new[..y_start],
            old_base,
            new_base,
            script,
            depth + 1,
        );
        for step in 0..(x_end - x_start) {
            script.push((
                Op::Keep,
                old_base + x_start + step,
                new_base + y_start + step,
            ));
        }
        diff_ops(
            &middle_old[x_end..],
            &middle_new[y_end..],
            old_base + x_end,
            new_base + y_end,
            script,
            depth + 1,
        );
    } else {
        // No overlap found: the whole middle is one replacement.
        for index in 0..middle_old.len() {
            script.push((Op::Remove, old_base + index, new_base));
        }
        for index in 0..middle_new.len() {
            script.push((Op::Add, old_base, new_base + index));
        }
    }

    for step in 0..suffix {
        script.push((
            Op::Keep,
            old_base + middle_old.len() + step,
            new_base + middle_new.len() + step,
        ));
    }
}

/// The maximal run of equal lines on the diagonal that both searches found.
///
/// The two searches only record how far each path travelled; the extreme point
/// of one path can sit past lines the other one matched (a pure insert or
/// delete), so the common run is grown and shrunk by direct comparison here.
/// Every line the caller keeps is then genuinely equal on both sides.
fn common_run(
    old: &[&str],
    new: &[&str],
    x_forward: isize,
    y_forward: isize,
    x_back: isize,
    y_back: isize,
) -> (usize, usize, usize, usize) {
    let (n, m) = (old.len() as isize, new.len() as isize);
    let (mut x_start, mut y_start) = (x_forward, y_forward);
    while x_start > x_back
        && y_start > y_back
        && old[(x_start - 1) as usize] == new[(y_start - 1) as usize]
    {
        x_start -= 1;
        y_start -= 1;
    }
    let (mut x_end, mut y_end) = (x_forward, y_forward);
    while x_end < n && y_end < m && old[x_end as usize] == new[y_end as usize] {
        x_end += 1;
        y_end += 1;
    }
    (
        x_start.max(0) as usize,
        y_start.max(0) as usize,
        x_end.max(0) as usize,
        y_end.max(0) as usize,
    )
}

/// Find the middle snake of the shortest edit script for `old`/`new`.
///
/// Returns `(x_start, y_start, x_end, y_end)`: the common run, in both inputs,
/// that every shortest script runs through. `None` means no overlap was found
/// within the diagonal budget and the caller should treat the whole input as
/// one replacement.
///
/// Memory: two `V` arrays of `(n + m)` entries. Time: O((n + m) * D).
fn middle_snake(old: &[&str], new: &[&str]) -> Option<(usize, usize, usize, usize)> {
    let n = old.len() as isize;
    let m = new.len() as isize;
    let max_d = (n + m + 1) / 2;
    let offset = max_d as usize;
    let length = (2 * max_d + 1) as usize;
    // `forward[k]` is the furthest x reached on diagonal k walking forward,
    // `backward[k]` the same walking backward from the end. -1 marks a
    // diagonal the search has not reached yet.
    let mut forward = vec![-1isize; length];
    let mut backward = vec![-1isize; length];
    forward[offset + 1] = 0;
    backward[offset + 1] = 0;
    let delta = n - m;
    let front_first = delta % 2 != 0;

    let (mut k1_start, mut k1_end) = (0isize, 0isize);
    let (mut k2_start, mut k2_end) = (0isize, 0isize);

    for d in 0..=max_d {
        // Forward search.
        let mut k1 = -d + k1_start;
        while k1 <= d - k1_end {
            let slot = (offset as isize + k1) as usize;
            let mut x = if k1 == -d || (k1 != d && forward[slot - 1] < forward[slot + 1]) {
                forward[slot + 1]
            } else {
                forward[slot - 1] + 1
            };
            let mut y = x - k1;
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            forward[slot] = x;
            if x > n {
                k1_end += 2;
            } else if y > m {
                k1_start += 2;
            } else if front_first
                && let Some((x_back, y_back)) =
                    backward_overlap(&backward, offset, length, delta, k1, n, m)
                && x >= x_back
            {
                return Some(common_run(old, new, x, y, x_back, y_back));
            }
            k1 += 2;
        }

        // Backward search.
        let mut k2 = -d + k2_start;
        while k2 <= d - k2_end {
            let slot = (offset as isize + k2) as usize;
            let mut x = if k2 == -d || (k2 != d && backward[slot - 1] < backward[slot + 1]) {
                backward[slot + 1]
            } else {
                backward[slot - 1] + 1
            };
            let mut y = x - k2;
            while x < n && y < m && old[(n - x - 1) as usize] == new[(m - y - 1) as usize] {
                x += 1;
                y += 1;
            }
            backward[slot] = x;
            if x > n {
                k2_end += 2;
            } else if y > m {
                k2_start += 2;
            } else if !front_first {
                let mirrored = (offset as isize + delta - k2) as usize;
                if mirrored < length && forward[mirrored] != -1 {
                    let x1 = forward[mirrored];
                    let y1 = x1 - (delta - k2);
                    let (x_back, y_back) = (n - x, m - y);
                    if x1 >= x_back {
                        return Some(common_run(old, new, x1, y1, x_back, y_back));
                    }
                }
            }
            k2 += 2;
        }
    }
    None
}

/// The absolute point the backward search has reached on the diagonal that
/// mirrors `k1`, if that diagonal is inside the `V` array.
fn backward_overlap(
    backward: &[isize],
    offset: usize,
    length: usize,
    delta: isize,
    k1: isize,
    n: isize,
    m: isize,
) -> Option<(isize, isize)> {
    let mirrored = offset as isize + delta - k1;
    if mirrored < 0 || mirrored >= length as isize {
        return None;
    }
    let x = backward[mirrored as usize];
    if x == -1 {
        return None;
    }
    let y = x - (delta - k1);
    if y > m {
        return None;
    }
    Some((n - x, m - y))
}

// ---------------------------------------------------------------------------
// Display diff
// ---------------------------------------------------------------------------

/// pi's `countLines` (`edit-diff.ts:437-439`): lines as
/// `text.split("\n").length` counts them.
fn count_lines(text: &str) -> usize {
    count_line_feeds(text, 0, text.len()) + 1
}

/// pi's `countLineFeeds` (`edit-diff.ts:441-447`).
fn count_line_feeds(text: &str, start: usize, end: usize) -> usize {
    let mut count = 0;
    let mut position = start;
    while let Some(index) = text[position..end].find('\n') {
        count += 1;
        position += index + 1;
    }
    count
}

/// pi's `formatDisplayDiff` (`edit-diff.ts:450-569`): format jsdiff line parts
/// as display diff lines, numbering from the given 1-based old and new lines.
fn format_display_diff(
    parts: &[DiffPart],
    old_start_line: usize,
    new_start_line: usize,
    line_num_width: usize,
    context_lines: usize,
) -> (Vec<String>, Option<usize>) {
    let mut output: Vec<String> = Vec::new();

    let mut old_line_num = old_start_line;
    let mut new_line_num = new_start_line;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    let pad = |number: usize| format!("{number:>line_num_width$}");

    for (index, part) in parts.iter().enumerate() {
        let mut raw: Vec<&str> = part.value.split('\n').collect();
        if raw.last() == Some(&"") {
            raw.pop();
        }

        if part.added || part.removed {
            // Capture the first changed line (in the new file).
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }
            // Show the change.
            for line in &raw {
                if part.added {
                    output.push(format!("+{} {line}", pad(new_line_num)));
                    new_line_num += 1;
                } else {
                    output.push(format!("-{} {line}", pad(old_line_num)));
                    old_line_num += 1;
                }
            }
            last_was_change = true;
        } else {
            // Context lines — only show a few before/after changes.
            let next_part_is_change =
                index < parts.len() - 1 && (parts[index + 1].added || parts[index + 1].removed);
            let has_leading_change = last_was_change;
            let has_trailing_change = next_part_is_change;

            if has_leading_change && has_trailing_change {
                if raw.len() <= context_lines * 2 {
                    for line in &raw {
                        output.push(format!(" {} {line}", pad(old_line_num)));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    let skipped_lines = raw.len() - context_lines * 2;
                    for line in &raw[..context_lines] {
                        output.push(format!(" {} {line}", pad(old_line_num)));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                    output.push(format!(" {} ...", " ".repeat(line_num_width)));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                    for line in &raw[raw.len() - context_lines..] {
                        output.push(format!(" {} {line}", pad(old_line_num)));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            } else if has_leading_change {
                let shown_lines = &raw[..context_lines.min(raw.len())];
                let skipped_lines = raw.len() - shown_lines.len();
                for line in shown_lines {
                    output.push(format!(" {} {line}", pad(old_line_num)));
                    old_line_num += 1;
                    new_line_num += 1;
                }
                if skipped_lines > 0 {
                    output.push(format!(" {} ...", " ".repeat(line_num_width)));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                }
            } else if has_trailing_change {
                let skipped_lines = raw.len().saturating_sub(context_lines);
                if skipped_lines > 0 {
                    output.push(format!(" {} ...", " ".repeat(line_num_width)));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                }
                for line in &raw[skipped_lines..] {
                    output.push(format!(" {} {line}", pad(old_line_num)));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                // Skip these context lines entirely.
                old_line_num += raw.len();
                new_line_num += raw.len();
            }

            last_was_change = false;
        }
    }

    (output, first_changed_line)
}

/// pi's `generateDiffString` (`edit-diff.ts:420-434`): display diff with line
/// numbers and context over the whole contents.
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> (String, Option<usize>) {
    let line_num_width = count_lines(old_content)
        .max(count_lines(new_content))
        .to_string()
        .len();
    let parts = diff_lines(old_content, new_content);
    let (output, first_changed_line) =
        format_display_diff(&parts, 1, 1, line_num_width, context_lines);
    (output.join("\n"), first_changed_line)
}

// ---------------------------------------------------------------------------
// Unified patch (jsdiff structuredPatch + formatPatch, FILE_HEADERS_ONLY)
// ---------------------------------------------------------------------------

/// jsdiff's `StructuredPatchHunk` (`edit-diff.ts` consumes
/// `Diff.structuredPatch(...).hunks`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredPatchHunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<String>,
}

/// jsdiff's `splitLines` (`dist/diff.js`): lines keep their trailing newline;
/// a last line without one stays without it.
fn js_split_lines(text: &str) -> Vec<String> {
    let has_trailing_nl = text.ends_with('\n');
    let mut result: Vec<String> = text.split('\n').map(|line| format!("{line}\n")).collect();
    if has_trailing_nl {
        result.pop();
    } else if let Some(last) = result.pop() {
        result.push(last[..last.len().saturating_sub(1)].to_string());
    }
    result
}

/// jsdiff's `structuredPatch` (`dist/diff.js`), reduced to the hunks.
pub fn structured_patch(
    old_content: &str,
    new_content: &str,
    context: usize,
) -> Vec<StructuredPatchHunk> {
    let mut diff = diff_lines(old_content, new_content);
    diff.push(DiffPart {
        added: false,
        removed: false,
        value: String::new(),
    }); // Append an empty value to make cleanup easier

    let hunks: Vec<StructuredPatchHunk> = {
        let mut hunks = Vec::new();
        let mut old_range_start = 0usize;
        let mut new_range_start = 0usize;
        let mut cur_range: Vec<String> = Vec::new();
        let mut old_line = 1usize;
        let mut new_line = 1usize;

        for (i, current) in diff.iter().enumerate() {
            let mut lines = js_split_lines(&current.value);
            if current.value.is_empty() {
                lines = Vec::new();
            }

            if current.added || current.removed {
                // If we have previous context, start with that.
                if old_range_start == 0 {
                    old_range_start = old_line;
                    new_range_start = new_line;
                    if i > 0 {
                        if context > 0 {
                            let prev_lines = js_split_lines(&diff[i - 1].value);
                            let start = prev_lines.len().saturating_sub(context);
                            for line in &prev_lines[start..] {
                                cur_range.push(format!(" {line}"));
                            }
                        }
                        let taken = cur_range.len();
                        old_range_start -= taken;
                        new_range_start -= taken;
                    }
                }
                // Output our changes.
                for line in &lines {
                    cur_range.push(format!("{}{line}", if current.added { '+' } else { '-' }));
                }
                // Track the updated file position.
                if current.added {
                    new_line += lines.len();
                } else {
                    old_line += lines.len();
                }
            } else {
                // Identical context lines. Track line changes.
                if old_range_start != 0 {
                    // Close out any changes that have been output (or join
                    // overlapping).
                    if lines.len() <= context * 2 && i < diff.len() - 2 {
                        // Overlapping.
                        for line in &lines {
                            cur_range.push(format!(" {line}"));
                        }
                    } else {
                        // End the range and output.
                        let context_size = lines.len().min(context);
                        for line in &lines[..context_size] {
                            cur_range.push(format!(" {line}"));
                        }
                        hunks.push(StructuredPatchHunk {
                            old_start: old_range_start,
                            old_lines: old_line - old_range_start + context_size,
                            new_start: new_range_start,
                            new_lines: new_line - new_range_start + context_size,
                            lines: std::mem::take(&mut cur_range),
                        });
                        old_range_start = 0;
                        new_range_start = 0;
                    }
                }

                old_line += lines.len();
                new_line += lines.len();
            }
        }
        hunks
    };

    // Step 2 (jsdiff): eliminate the trailing `\n` from each line of each
    // hunk, and, where needed, add `\ No newline at end of file`.
    let mut hunks = hunks;
    for hunk in &mut hunks {
        let mut lines: Vec<String> = Vec::with_capacity(hunk.lines.len());
        for line in hunk.lines.drain(..) {
            if line.ends_with('\n') {
                lines.push(line[..line.len() - 1].to_string());
            } else {
                lines.push(line);
                lines.push("\\ No newline at end of file".to_string());
            }
        }
        hunk.lines = lines;
    }
    hunks
}

/// jsdiff's `formatPatch` (`dist/diff.js`) with `FILE_HEADERS_ONLY`: the two
/// file headers, then the hunks. `old_header`/`new_header` are pi's
/// `undefined`, so no tab suffix is appended.
pub fn format_patch(path: &str, hunks: &[StructuredPatchHunk]) -> String {
    let mut ret: Vec<String> = Vec::new();
    ret.push(format!("--- {path}"));
    ret.push(format!("+++ {path}"));

    for hunk in hunks {
        // Unified Diff Format quirk: if the chunk size is 0, the first number
        // is one lower than one would expect.
        let (old_start, new_start) = if hunk.old_lines == 0 || hunk.new_lines == 0 {
            (
                hunk.old_start
                    .saturating_sub(usize::from(hunk.old_lines == 0)),
                hunk.new_start
                    .saturating_sub(usize::from(hunk.new_lines == 0)),
            )
        } else {
            (hunk.old_start, hunk.new_start)
        };
        ret.push(format!(
            "@@ -{old_start},{} +{new_start},{} @@",
            hunk.old_lines, hunk.new_lines
        ));
        ret.extend(hunk.lines.iter().cloned());
    }

    format!("{}\n", ret.join("\n"))
}

/// pi's `generateUnifiedPatch` (`edit-diff.ts:409-414`): jsdiff's
/// `createTwoFilesPatch(path, path, old, new, undefined, undefined,
/// { context, headerOptions: FILE_HEADERS_ONLY })`.
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    let hunks = structured_patch(old_content, new_content, context_lines);
    format_patch(path, &hunks)
}

// ---------------------------------------------------------------------------
// Windowed edit diff
// ---------------------------------------------------------------------------

/// pi's `DIFF_WINDOW_MARGIN_LINES` (`edit-diff.ts:582`): unchanged lines
/// diffed on each side of a replacement. Well above the 4 context lines
/// shown, so jsdiff aligns a change within its window as it would over the
/// whole file.
const DIFF_WINDOW_MARGIN_LINES: usize = 32;

/// pi's `DiffWindow` (`edit-diff.ts:629-636`): matching ranges of the old and
/// new contents around one or more replacements — offsets and 0-based first
/// lines.
#[derive(Debug, Clone, Copy)]
struct DiffWindow {
    old_start: usize,
    old_end: usize,
    old_line: usize,
    new_start: usize,
    new_end: usize,
    new_line: usize,
}

/// Start of the line that contains `offset` (`edit-diff.ts:639-642`).
fn line_start_before(text: &str, offset: usize) -> usize {
    // lastIndexOf treats a negative position as 0 and would find a line feed
    // at offset 0.
    if offset == 0 {
        return 0;
    }
    match text[..offset].rfind('\n') {
        Some(index) => index + 1,
        None => 0,
    }
}

/// pi's `getDiffWindows` (`edit-diff.ts:645-687`): whole-line windows with
/// margin around the replacements, which are sorted and disjoint; overlapping
/// windows merge.
fn get_diff_windows(base_content: &str, replacements: &[TextReplacement]) -> Vec<DiffWindow> {
    let mut windows: Vec<DiffWindow> = Vec::new();
    // Offset and line deltas of the replacements before the current one, and
    // a line-counting cursor.
    let mut offset_delta = 0isize;
    let mut line_delta = 0isize;
    let mut cursor_offset = 0usize;
    let mut cursor_line = 0isize;
    for replacement in replacements {
        let match_end = replacement.match_index + replacement.match_length;
        let mut start = line_start_before(base_content, replacement.match_index);
        for _ in 0..DIFF_WINDOW_MARGIN_LINES {
            if start == 0 {
                break;
            }
            start = line_start_before(base_content, start - 1);
        }
        let mut end = match_end;
        for i in 0..=DIFF_WINDOW_MARGIN_LINES {
            if end >= base_content.len() {
                break;
            }
            let from = if i == 0 {
                match_end.saturating_sub(1).max(replacement.match_index)
            } else {
                end
            };
            end = match base_content[from..].find('\n') {
                Some(relative) => from + relative + 1,
                None => base_content.len(),
            };
        }
        let replacement_offset_delta =
            replacement.new_text.len() as isize - replacement.match_length as isize;
        let replacement_line_delta =
            count_line_feeds(&replacement.new_text, 0, replacement.new_text.len()) as isize
                - count_line_feeds(base_content, replacement.match_index, match_end) as isize;

        let merged = match windows.last_mut() {
            Some(previous) if start <= previous.old_end => {
                previous.old_end = previous.old_end.max(end);
                previous.new_end =
                    (previous.old_end as isize + offset_delta + replacement_offset_delta) as usize;
                true
            }
            _ => false,
        };
        if !merged {
            cursor_line += count_line_feeds(base_content, cursor_offset, start) as isize;
            cursor_offset = start;
            windows.push(DiffWindow {
                old_start: start,
                old_end: end,
                old_line: cursor_line.max(0) as usize,
                new_start: (start as isize + offset_delta) as usize,
                new_end: (end as isize + offset_delta + replacement_offset_delta) as usize,
                new_line: (cursor_line + line_delta) as usize,
            });
        }
        offset_delta += replacement_offset_delta;
        line_delta += replacement_line_delta;
    }
    windows
}

/// pi's `generateEditDiff` (`edit-diff.ts:589-626`): diff an edit only around
/// its replacements, so time and memory follow the size of the change instead
/// of the file. The result matches `generate_diff_string` and
/// `generate_unified_patch` over the whole contents, except that jsdiff may
/// align runs of repeated lines differently within a window.
pub fn generate_edit_diff(
    path: &str,
    base_content: &str,
    new_content: &str,
    replacements: &[TextReplacement],
    context_lines: usize,
) -> EditDiff {
    let line_num_width = count_lines(base_content)
        .max(count_lines(new_content))
        .to_string()
        .len();
    let skip_marker = format!(" {} ...", " ".repeat(line_num_width));
    let mut output: Vec<String> = Vec::new();
    let mut hunks: Vec<StructuredPatchHunk> = Vec::new();
    let mut first_changed_line: Option<usize> = None;
    for window in get_diff_windows(base_content, replacements) {
        // jsdiff's caller slices with `String.slice`, which clamps to the
        // content length; Rust would panic on an out-of-range end.
        let old_text = &base_content
            [window.old_start.min(base_content.len())..window.old_end.min(base_content.len())];
        let new_text = &new_content
            [window.new_start.min(new_content.len())..window.new_end.min(new_content.len())];
        let parts = diff_lines(old_text, new_text);
        let (shown_output, shown_first) = format_display_diff(
            &parts,
            window.old_line + 1,
            window.new_line + 1,
            line_num_width,
            context_lines,
        );
        if first_changed_line.is_none() {
            first_changed_line = shown_first;
        }
        // Both windows around a run of unchanged lines mark it as skipped; the
        // whole-file diff shows one marker.
        let mut shown_output = shown_output;
        if output.last().is_some_and(|line| line == &skip_marker)
            && shown_output
                .first()
                .is_some_and(|line| line == &skip_marker)
        {
            shown_output.remove(0);
        }
        // A loop, not a spread: pi avoids overflowing the call stack on a
        // replacement of many lines (`edit-diff.ts:615`).
        for line in shown_output {
            output.push(line);
        }
        let window_patch = structured_patch(old_text, new_text, context_lines);
        for mut hunk in window_patch {
            hunk.old_start += window.old_line;
            hunk.new_start += window.new_line;
            hunks.push(hunk);
        }
    }
    let patch_text = format_patch(path, &hunks);
    EditDiff {
        diff: output.join("\n"),
        patch: patch_text,
        first_changed_line,
    }
}
