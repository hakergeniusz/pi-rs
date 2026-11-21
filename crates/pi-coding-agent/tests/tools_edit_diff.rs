//! Ports of the edit-diff assertions from `tools.test.ts` (apply + CRLF
//! sections) and `edit-diff-window.test.ts` (windowed vs whole-file diff).

use pi_coding_agent::tools::edit_diff::{
    Edit, TextReplacement, apply_edits_to_normalized_content, apply_replacements_to_original,
    fuzzy_find_text, generate_diff_string, generate_edit_diff, generate_unified_patch,
    normalize_for_fuzzy_match, normalize_to_lf,
};
use std::fmt::Write as _;

fn edit(old_text: &str, new_text: &str) -> Edit {
    Edit {
        old_text: old_text.to_string(),
        new_text: new_text.to_string(),
    }
}

fn replacement(match_index: usize, match_length: usize, new_text: &str) -> TextReplacement {
    TextReplacement {
        match_index,
        match_length,
        new_text: new_text.to_string(),
    }
}

// --- tools.test.ts, "edit tool CRLF handling" -------------------------------

#[test]
fn preserves_crlf_line_endings_after_edit() {
    let applied = apply_edits_to_normalized_content(
        "first\nsecond\nthird\n",
        &[edit("second\n", "REPLACED\n")],
        "f.txt",
    )
    .expect("applies");
    // The whole file is LF, so the normalized content is written directly.
    assert_eq!(applied.new_content, "first\nREPLACED\nthird\n");
}

#[test]
fn applies_replacements_to_original_preserves_crlf() {
    // edit.ts: `bom + (content === normalized ? newContent :
    // applyReplacementsToOriginal(content, replacements))` — a CRLF file
    // takes the splice path. Replacement coordinates are the normalized
    // text's, so lengths are the LF forms ("second\n", not "second\r\n").
    let written = apply_replacements_to_original(
        "first\r\nsecond\r\nthird\r\n",
        &[replacement("first\n".len(), "second\n".len(), "REPLACED\n")],
    );
    assert_eq!(written, "first\r\nREPLACED\r\nthird\r\n");
}

#[test]
fn applies_replacements_to_original_preserves_lf() {
    let written = apply_replacements_to_original(
        "first\nsecond\nthird\n",
        &[replacement("first\n".len(), "second\n".len(), "REPLACED\n")],
    );
    assert_eq!(written, "first\nREPLACED\nthird\n");
}

#[test]
fn writes_crlf_for_inserted_lines_in_crlf_files() {
    // "first\r\nsecond\r\nthird", oldText "second\nthird" (normalized),
    // newText "2a\r\n2b\nthird!" → the written file keeps CRLF.
    let original = "first\r\nsecond\r\nthird";
    let normalized = normalize_to_lf(original);
    let applied = apply_edits_to_normalized_content(
        &normalized,
        &[edit("second\nthird", "2a\r\n2b\nthird!")],
        "f.txt",
    )
    .expect("applies");
    let written = apply_replacements_to_original(original, &applied.replacements);
    assert_eq!(written, "first\r\n2a\r\n2b\r\nthird!");
}

#[test]
fn writes_lf_for_inserted_lines_in_lf_files() {
    let original = "first\nsecond\nthird";
    let applied = apply_edits_to_normalized_content(
        original,
        &[edit("second\nthird", "2a\r\n2b\nthird!")],
        "f.txt",
    )
    .expect("applies");
    // The original is the normalized text, so newContent is written as-is.
    // newText is normalized to LF first (`edit-diff.ts:348-351`), matching
    // tools.test.ts "should write LF for inserted lines in LF files".
    assert_eq!(applied.new_content, "first\n2a\n2b\nthird!");
}

#[test]
fn keeps_crlf_lines_when_the_first_line_ends_with_lf() {
    // #8544: "a\nb\r\nX = 1\r\n", oldText "X = 1" → "a\nb\r\nX = 2\r\n".
    let original = "a\nb\r\nX = 1\r\n";
    let normalized = normalize_to_lf(original);
    let applied =
        apply_edits_to_normalized_content(&normalized, &[edit("X = 1", "X = 2")], "f.txt")
            .expect("applies");
    assert_eq!(applied.new_content, "a\nb\nX = 2\n");
    let written = apply_replacements_to_original(original, &applied.replacements);
    assert_eq!(written, "a\nb\r\nX = 2\r\n");
}

#[test]
fn keeps_lf_lines_when_the_first_line_ends_with_crlf() {
    // #8544: "a\r\nb\nc", oldText "c" → "a\r\nb\nC".
    let original = "a\r\nb\nc";
    let normalized = normalize_to_lf(original);
    let applied = apply_edits_to_normalized_content(&normalized, &[edit("c", "C")], "f.txt")
        .expect("applies");
    let written = apply_replacements_to_original(original, &applied.replacements);
    assert_eq!(written, "a\r\nb\nC");
}

#[test]
fn keeps_a_lone_cr_in_an_untouched_line() {
    // #8544: "a\rb\nX = 1\n" → "a\rb\nX = 2\n".
    let original = "a\rb\nX = 1\n";
    let normalized = normalize_to_lf(original);
    let applied =
        apply_edits_to_normalized_content(&normalized, &[edit("X = 1", "X = 2")], "f.txt")
            .expect("applies");
    let written = apply_replacements_to_original(original, &applied.replacements);
    assert_eq!(written, "a\rb\nX = 2\n");
}

#[test]
fn gives_inserted_lines_the_line_ending_of_the_line_they_replace() {
    // #8544: "lf1\nlf2\ncr1\r\ncr2\r\n" with two edits.
    let original = "lf1\nlf2\ncr1\r\ncr2\r\n";
    let normalized = normalize_to_lf(original);
    let applied = apply_edits_to_normalized_content(
        &normalized,
        &[edit("lf1\n", "A\nB\n"), edit("cr1\n", "C\nD\n")],
        "f.txt",
    )
    .expect("applies");
    let written = apply_replacements_to_original(original, &applied.replacements);
    assert_eq!(written, "A\nB\nlf2\nC\r\nD\r\ncr2\r\n");
}

#[test]
fn keeps_untouched_line_endings_when_fuzzy_matching_widens_the_edit() {
    // #8544: "a\r\nfoo   \r\nb\n", oldText "foo\n" → "a\r\nFOO\r\nb\n".
    let original = "a\r\nfoo   \r\nb\n";
    let normalized = normalize_to_lf(original);
    let applied =
        apply_edits_to_normalized_content(&normalized, &[edit("foo\n", "FOO\n")], "f.txt")
            .expect("applies");
    let written = apply_replacements_to_original(original, &applied.replacements);
    assert_eq!(written, "a\r\nFOO\r\nb\n");
}

#[test]
fn does_not_merge_a_kept_lone_cr_with_a_line_feed_after_it() {
    // The CR-seam cases from tools.test.ts: a kept CR and an LF meeting at a
    // splice seam must not merge into one CRLF line break.
    // Case 0: inserted text starting with LF after a kept lone CR.
    let original = "a\rb\nc";
    let normalized = normalize_to_lf(original);
    let applied = apply_edits_to_normalized_content(&normalized, &[edit("b", "\nB")], "f.txt")
        .expect("applies");
    let written = apply_replacements_to_original(original, &applied.replacements);
    assert_eq!(written, "a\r\n\nB\nc");
    // Case 1: deleted text between a kept lone CR and a kept LF.
    let applied =
        apply_edits_to_normalized_content(&normalized, &[edit("b", "")], "f.txt").expect("applies");
    let written = apply_replacements_to_original(original, &applied.replacements);
    assert_eq!(written, "a\r\n\nc");
}

#[test]
fn does_not_merge_an_inserted_cr_line_ending_with_a_kept_line_feed_after_it() {
    // tools.test.ts checks the seam directly on applyReplacementsToOriginal.
    let written = apply_replacements_to_original("a\rb\nc", &[replacement(2, 1, "\r")]);
    assert_eq!(normalize_to_lf(&written), "a\n\n\nc");
}

#[test]
fn writes_content_that_normalizes_to_the_diffed_content_for_random_mixed_endings() {
    // The TS property test: normalizeToLF(applyReplacementsToOriginal(
    //   original, applied.replacements)) === applied.newContent.
    let mut seed: u64 = 1;
    let mut random = |n: u64| -> u64 {
        seed = (seed * 1_103_515_245 + 12_345) & 0x7fff_ffff;
        seed * n / 0x8000_0000
    };
    let endings = ["\n", "\r\n", "\r", ""];
    for _ in 0..3000 {
        let mut original = String::new();
        let lines = random(12) + 1;
        for _ in 0..lines {
            let _ = write!(original, "w{}", random(100));
            if random(5) == 0 {
                original.push_str("  ");
            }
            original.push_str(endings[random(4) as usize]);
        }
        let normalized = normalize_to_lf(&original);
        let start = random(normalized.chars().count() as u64) as usize;
        let text: Vec<char> = normalized.chars().collect();
        if start >= text.len() {
            continue;
        }
        let length = (1 + random(12)) as usize;
        let old_text: String = text.iter().skip(start).take(length).collect();
        let new_text = ["", "\n", "X\n", "\nY", "\r", "Z"][random(6) as usize];
        let Ok(applied) =
            apply_edits_to_normalized_content(&normalized, &[edit(&old_text, new_text)], "f.txt")
        else {
            continue; // duplicate, not found, or no change
        };
        let written = apply_replacements_to_original(&original, &applied.replacements);
        assert_eq!(
            normalize_to_lf(&written),
            applied.new_content,
            "original: {original:?}"
        );
    }
}

// --- edit-diff-window.test.ts -----------------------------------------------

/// Small deterministic generator (mulberry32), using the high bits — the
/// TS test's `createRandom`, ported operation for operation.
fn create_random(seed: i32) -> impl FnMut(usize) -> usize {
    let mut state = seed;
    move |max: usize| {
        state = state.wrapping_add(0x6d2b_79f5);
        let mut t = (state ^ ((state as u32 >> 15) as i32)).wrapping_mul(1i32.wrapping_add(state));
        t = (t.wrapping_add((t ^ ((t as u32 >> 7) as i32)).wrapping_mul(0x3d | t))) ^ t;
        let mixed = (t as u32) ^ ((t as u32) >> 14);
        ((mixed as f64 / 4_294_967_296.0) * max as f64).floor() as usize
    }
}

fn apply_replacements(content: &str, replacements: &[TextReplacement]) -> String {
    let mut result = String::new();
    let mut position = 0usize;
    for replacement in replacements {
        result.push_str(&content[position..replacement.match_index]);
        result.push_str(&replacement.new_text);
        position = replacement.match_index + replacement.match_length;
    }
    result.push_str(&content[position..]);
    result
}

#[test]
fn windowed_diff_matches_the_whole_file_diff_when_lines_are_distinct() {
    let mut random = create_random(1);
    for run in 0..200 {
        let line_count = random(400);
        let lines: Vec<String> = (0..line_count).map(|i| format!("line {i}")).collect();
        let content = format!(
            "{}{}",
            lines.join("\n"),
            if random(2) == 0 { "\n" } else { "" }
        );
        let mut replacements: Vec<TextReplacement> = Vec::new();
        let mut position = 0usize;
        let count = 1 + random(4);
        for i in 0..count {
            if position >= content.len() {
                break;
            }
            let window = (content.len() - position).min(300);
            let match_index = position + random(window);
            if match_index >= content.len() {
                break;
            }
            let length_window = (content.len() - match_index).min(120);
            let match_length = 1 + random(length_window);
            let new_count = random(4);
            let new_lines: Vec<String> = (0..new_count).map(|j| format!("new {i} {j}")).collect();
            let new_text = if random(3) == 0 {
                String::new()
            } else {
                format!(
                    "{}{}",
                    new_lines.join("\n"),
                    if random(2) == 0 { "\n" } else { "" }
                )
            };
            replacements.push(replacement(match_index, match_length, &new_text));
            position = match_index + match_length;
        }
        let new_content = apply_replacements(&content, &replacements);
        if content == new_content {
            continue;
        }
        let windowed = generate_edit_diff("f.txt", &content, &new_content, &replacements, 4);
        let (whole_diff, whole_first) = generate_diff_string(&content, &new_content, 4);
        let whole_patch = generate_unified_patch("f.txt", &content, &new_content, 4);
        assert_eq!(
            (
                windowed.diff.as_str(),
                windowed.first_changed_line,
                windowed.patch.as_str()
            ),
            (whole_diff.as_str(), whole_first, whole_patch.as_str()),
            "run {run}"
        );
    }
}

#[test]
fn patch_is_parseable_unified_diff() {
    let applied =
        apply_edits_to_normalized_content("Hello, world!\n", &[edit("world", "testing")], "f.txt")
            .expect("applies");
    let edit_diff = generate_edit_diff(
        "f.txt",
        &applied.base_content,
        &applied.new_content,
        &applied.replacements,
        4,
    );
    assert!(edit_diff.patch.contains("--- "));
    assert!(edit_diff.patch.contains("+++ "));
    assert!(edit_diff.patch.contains("@@"));
    assert!(edit_diff.patch.contains("-Hello, world!"));
    assert!(edit_diff.patch.contains("+Hello, testing!"));
}

// --- Full Unicode NFKC for fuzzy matching (edit-diff.ts:77-98) ---------------

#[test]
fn nfkc_normalizes_fullwidth_forms() {
    assert_eq!(normalize_for_fuzzy_match("ＡＢＣ！"), "ABC!");
    assert_eq!(normalize_for_fuzzy_match("ｆｏｏ　ｂａｒ"), "foo bar");
}

#[test]
fn nfkc_normalizes_ligatures() {
    assert_eq!(normalize_for_fuzzy_match("ﬁle"), "file");
    assert_eq!(normalize_for_fuzzy_match("ﬂow ﬀ ﬃ"), "flow ff ffi");
}

#[test]
fn nfkc_composes_precomposed_and_combining_marks_alike() {
    // The old decomposition-only subset produced `cafe\u{301}` for both;
    // full NFKC composes both into the same `café`.
    assert_eq!(
        normalize_for_fuzzy_match("café"),
        normalize_for_fuzzy_match("cafe\u{301}")
    );
    assert_eq!(normalize_for_fuzzy_match("café"), "café");
}

#[test]
fn nfkc_reorders_combining_marks_canonically() {
    // U+032B (ccc 220) must sort before U+0301 (ccc 230); the two spellings are
    // canonically equivalent and full NFKC gives them identical output.
    assert_eq!(
        normalize_for_fuzzy_match("a\u{301}\u{32b}"),
        normalize_for_fuzzy_match("a\u{32b}\u{301}")
    );
}

#[test]
fn nfkc_normalizes_compatibility_forms_outside_the_old_subset() {
    // Roman numerals and astral mathematical letters were absent from the
    // hand-transcribed tables; full NFKC handles both.
    assert_eq!(normalize_for_fuzzy_match("\u{2160}"), "I");
    assert_eq!(normalize_for_fuzzy_match("\u{1d400}\u{1d41b}"), "Ab");
}

#[test]
fn fuzzy_normalization_keeps_smart_quote_and_dash_mappings() {
    assert_eq!(normalize_for_fuzzy_match("\u{2018}hi\u{2019}"), "'hi'");
    assert_eq!(normalize_for_fuzzy_match("\u{201c}hi\u{201d}"), "\"hi\"");
    assert_eq!(normalize_for_fuzzy_match("a\u{2011}b"), "a-b");
    assert_eq!(
        normalize_for_fuzzy_match("a\u{2013}b\u{2014}c\u{2212}d"),
        "a-b-c-d"
    );
}

#[test]
fn fuzzy_normalization_strips_trailing_whitespace_and_special_spaces() {
    assert_eq!(
        normalize_for_fuzzy_match("first   \nsecond\t\nthird"),
        "first\nsecond\nthird"
    );
    assert_eq!(normalize_for_fuzzy_match("a\u{00a0}b\u{2003}c"), "a b c");
}

#[test]
fn exact_ascii_substring_wins_over_fuzzy_matching() {
    let matched = fuzzy_find_text("hello world\n", "world");
    assert!(matched.found);
    assert!(!matched.used_fuzzy_match);
    assert_eq!(matched.index, 6);
    assert_eq!(matched.match_length, 5);
    assert_eq!(matched.content_for_replacement, "hello world\n");
}

#[test]
fn no_fuzzy_fallback_when_normalized_text_still_differs() {
    let matched = fuzzy_find_text("alpha beta", "gamma");
    assert!(!matched.found);
    assert_eq!(matched.index, 0);
    assert_eq!(matched.match_length, 0);
    assert!(!matched.used_fuzzy_match);
    assert_eq!(matched.content_for_replacement, "alpha beta");
}

#[test]
fn fullwidth_old_text_matches_ascii_file_through_fuzzy_path() {
    let applied = apply_edits_to_normalized_content(
        "let x = 1;\n",
        &[edit("ｌｅｔ ｘ ＝ １;", "let x = 2;")],
        "f.txt",
    )
    .expect("applies");
    assert_eq!(applied.new_content, "let x = 2;\n");
}

#[test]
fn ligature_and_smart_quote_old_texts_match_through_fuzzy_path() {
    let ligature = fuzzy_find_text("the file is here", "ﬁle");
    assert!(ligature.found);
    assert!(ligature.used_fuzzy_match);

    let quotes = fuzzy_find_text("He said \"hi\" - ok", "\u{201c}hi\u{201d} \u{2014} ok");
    assert!(quotes.found);
    assert!(quotes.used_fuzzy_match);
}

#[test]
fn decomposed_old_text_matches_precomposed_file_through_fuzzy_path() {
    let matched = fuzzy_find_text("café time", "cafe\u{301}");
    assert!(matched.found);
    assert!(matched.used_fuzzy_match);
}
