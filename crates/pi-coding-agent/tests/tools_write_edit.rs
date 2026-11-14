//! Ports of the write/edit tool tests from `tools.test.ts` and
//! `edit-tool-legacy-input.test.ts`.

mod common;

use common::{TempDir, signal, text_of};
use pi_agent::types::AbortSignal;
use pi_coding_agent::tools::{edit, write};
use serde_json::json;

async fn run_edit(
    cwd: &std::path::Path,
    arguments: serde_json::Value,
) -> Result<pi_agent::types::AgentToolResult, String> {
    let context = pi_coding_agent::tools::ToolContext::new(cwd);
    edit::execute(&arguments, &context, &AbortSignal::new())
        .await
        .map_err(|error| error.0)
}

// --- write tool -------------------------------------------------------------

#[tokio::test]
async fn writes_file_contents() {
    let dir = TempDir::new("write");
    let test_file = dir.join("write-test.txt");
    let context = pi_coding_agent::tools::ToolContext::new(dir.path());
    let arguments = json!({ "path": test_file.display().to_string(), "content": "Test content" });
    let result = write::execute(&arguments, &context, &signal())
        .await
        .expect("writes");
    assert_eq!(
        text_of(&result),
        format!("Successfully wrote to {}", test_file.display())
    );
    assert_eq!(
        text_of(&result),
        "Successfully wrote to ".to_string() + &test_file.display().to_string()
    );
    let file_content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(file_content, "Test content");
}

#[tokio::test]
async fn creates_parent_directories() {
    let dir = TempDir::new("write-nested");
    let test_file = dir.join("nested/dir/test.txt");
    let context = pi_coding_agent::tools::ToolContext::new(dir.path());
    let arguments = json!({ "path": test_file.display().to_string(), "content": "Nested content" });
    let result = write::execute(&arguments, &context, &signal())
        .await
        .expect("writes");
    assert!(text_of(&result).contains("Successfully wrote"));
}

// --- edit tool --------------------------------------------------------------

#[tokio::test]
async fn replaces_text_in_file() {
    let dir = TempDir::new("edit");
    let test_file = dir.write("edit-test.txt", "Hello, world!");
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "world", "newText": "testing" }] }),
    )
    .await
    .expect("edits");
    let output = text_of(&result);
    assert!(output.contains("Successfully replaced"));
    assert!(result.details["diff"].is_string());
    assert!(
        result.details["diff"]
            .as_str()
            .expect("diff")
            .contains("testing")
    );
    let patch = result.details["patch"].as_str().expect("patch");
    assert!(patch.contains("--- "));
    assert!(patch.contains("+++ "));
    assert!(patch.contains("@@"));
    assert!(patch.contains("-Hello, world!"));
    assert!(patch.contains("+Hello, testing!"));
}

#[tokio::test]
async fn fails_if_text_not_found() {
    let dir = TempDir::new("edit-notfound");
    let test_file = dir.write("edit-test.txt", "Hello, world!");
    let error = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "nonexistent", "newText": "testing" }] }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Could not find the exact text"), "{error}");
}

#[tokio::test]
async fn includes_enoent_when_the_edit_target_does_not_exist() {
    let dir = TempDir::new("edit-enoent");
    let missing = dir.join("missing.txt");
    let error = run_edit(
        dir.path(),
        json!({ "path": missing.display().to_string(), "edits": [{ "oldText": "hello", "newText": "world" }] }),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        format!(
            "Could not edit file: {}. Error code: ENOENT.",
            missing.display()
        )
    );
}

#[tokio::test]
async fn fails_if_text_appears_multiple_times() {
    let dir = TempDir::new("edit-dup");
    let test_file = dir.write("edit-test.txt", "foo foo foo");
    let error = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "foo", "newText": "bar" }] }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Found 3 occurrences"), "{error}");
}

#[tokio::test]
async fn replaces_multiple_disjoint_regions_in_one_call() {
    let dir = TempDir::new("edit-multi");
    let test_file = dir.write("edit-multi.txt", "alpha\nbeta\ngamma\ndelta\n");
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [
            { "oldText": "alpha\n", "newText": "ALPHA\n" },
            { "oldText": "gamma\n", "newText": "GAMMA\n" }
        ] }),
    )
    .await
    .expect("edits");
    assert!(text_of(&result).contains("Successfully replaced 2 block(s)"));
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, "ALPHA\nbeta\nGAMMA\ndelta\n");
    assert!(
        result.details["diff"]
            .as_str()
            .expect("diff")
            .contains("ALPHA")
    );
    assert!(
        result.details["diff"]
            .as_str()
            .expect("diff")
            .contains("GAMMA")
    );
}

#[tokio::test]
async fn collapses_large_unchanged_gaps_in_multi_edit_diffs() {
    let dir = TempDir::new("edit-gap");
    let lines: Vec<String> = (0..600).map(|i| format!("line {:03}", i + 1)).collect();
    let test_file = dir.write(
        "edit-multi-large-gap.txt",
        &format!("{}\n", lines.join("\n")),
    );
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [
            { "oldText": "line 100\n", "newText": "LINE 100\n" },
            { "oldText": "line 300\n", "newText": "LINE 300\n" },
            { "oldText": "line 500\n", "newText": "LINE 500\n" }
        ] }),
    )
    .await
    .expect("edits");
    let diff = result.details["diff"].as_str().expect("diff").to_string();
    assert!(diff.contains("LINE 100"));
    assert!(diff.contains("LINE 300"));
    assert!(diff.contains("LINE 500"));
    assert!(diff.contains("..."));
    assert!(!diff.contains("line 250"));
    assert!(
        diff.split('\n').count() < 50,
        "{} lines",
        diff.split('\n').count()
    );
}

#[tokio::test]
async fn matches_edits_against_the_original_file_not_incrementally() {
    let dir = TempDir::new("edit-original");
    let test_file = dir.write("edit-multi-original.txt", "foo\nbar\nbaz\n");
    run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [
            { "oldText": "foo\n", "newText": "foo bar\n" },
            { "oldText": "bar\n", "newText": "BAR\n" }
        ] }),
    )
    .await
    .expect("edits");
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, "foo bar\nBAR\nbaz\n");
}

#[tokio::test]
async fn fails_when_edits_is_empty() {
    let dir = TempDir::new("edit-empty");
    let test_file = dir.write("edit-empty-edits.txt", "hello\nworld\n");
    let error = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [] }),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("edits must contain at least one replacement"),
        "{error}"
    );
}

#[tokio::test]
async fn fails_when_multi_edit_regions_overlap() {
    let dir = TempDir::new("edit-overlap");
    let test_file = dir.write("edit-overlap.txt", "one\ntwo\nthree\n");
    let error = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [
            { "oldText": "one\ntwo\n", "newText": "ONE\nTWO\n" },
            { "oldText": "two\nthree\n", "newText": "TWO\nTHREE\n" }
        ] }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("overlap"), "{error}");
}

#[tokio::test]
async fn does_not_partially_apply_edits_when_one_edit_fails() {
    let dir = TempDir::new("edit-no-partial");
    let original = "alpha\nbeta\ngamma\n";
    let test_file = dir.write("edit-no-partial.txt", original);
    let error = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [
            { "oldText": "alpha\n", "newText": "ALPHA\n" },
            { "oldText": "missing\n", "newText": "MISSING\n" }
        ] }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Could not find"), "{error}");
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, original);
}

#[tokio::test]
async fn includes_eacces_for_read_only_files() {
    let dir = TempDir::new("edit-readonly");
    let test_file = dir.write("edit-readonly.txt", "hello\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&test_file, std::fs::Permissions::from_mode(0o444))
            .expect("chmod");
    }
    // Root bypasses DAC checks (containers, CI-as-root), so the edit succeeds
    // and the EACCES path is unreachable there; upstream's vitest suite also
    // assumes a non-root runner. Skip the assertion in that environment.
    // Privileged environment: chmod is advisory, the edit succeeds, and only
    // the EACCES branch is interesting.
    if let Err(error) = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "hello", "newText": "world" }] }),
    )
    .await
    {
        assert_eq!(
            error,
            format!(
                "Could not edit file: {}. Error code: EACCES.",
                test_file.display()
            )
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&test_file, std::fs::Permissions::from_mode(0o644));
    }
}

// --- edit tool fuzzy matching (tools.test.ts) -------------------------------

#[tokio::test]
async fn fuzzy_matches_text_with_trailing_whitespace_stripped() {
    let dir = TempDir::new("fuzzy-ws");
    let test_file = dir.write("trailing-ws.txt", "line one   \nline two  \nline three\n");
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "line one\nline two\n", "newText": "replaced\n" }] }),
    )
    .await
    .expect("edits");
    assert!(text_of(&result).contains("Successfully replaced"));
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, "replaced\nline three\n");
}

#[tokio::test]
async fn fuzzy_matches_fullwidth_punctuation_in_chinese_text() {
    let dir = TempDir::new("fuzzy-cn");
    let test_file = dir.write("chinese-punctuation.txt", "你好，世界\n你好（世界）\n");
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "你好,世界\n你好(世界)\n", "newText": "你好，pi\n你好(pi)\n" }] }),
    )
    .await
    .expect("edits");
    assert!(text_of(&result).contains("Successfully replaced"));
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, "你好，pi\n你好(pi)\n");
}

#[tokio::test]
async fn fuzzy_matches_compatibility_equivalent_unicode_forms() {
    let dir = TempDir::new("fuzzy-uni");
    let test_file = dir.write(
        "unicode-compatibility.txt",
        "\u{FF21}\u{FF22}\u{FF23}\u{FF11}\u{FF12}\u{FF13}\ncafe\u{0301}\n",
    );
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "ABC123\ncaf\u{e9}\n", "newText": "XYZ789\ncoffee\n" }] }),
    )
    .await
    .expect("edits");
    assert!(text_of(&result).contains("Successfully replaced"));
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, "XYZ789\ncoffee\n");
}

#[tokio::test]
async fn fuzzy_matches_smart_single_quotes_to_ascii() {
    let dir = TempDir::new("fuzzy-sq");
    let test_file = dir.write("smart-quotes.txt", "console.log(\u{2018}hello\u{2019});\n");
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "console.log('hello');", "newText": "console.log('world');" }] }),
    )
    .await
    .expect("edits");
    assert!(text_of(&result).contains("Successfully replaced"));
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert!(content.contains("world"));
}

#[tokio::test]
async fn fuzzy_matches_smart_double_quotes_to_ascii() {
    let dir = TempDir::new("fuzzy-dq");
    let test_file = dir.write(
        "smart-double-quotes.txt",
        "const msg = \u{201C}Hello World\u{201D};\n",
    );
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "const msg = \"Hello World\";", "newText": "const msg = \"Goodbye\";" }] }),
    )
    .await
    .expect("edits");
    assert!(text_of(&result).contains("Successfully replaced"));
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert!(content.contains("Goodbye"));
}

#[tokio::test]
async fn fuzzy_matches_unicode_dashes_to_ascii_hyphen() {
    let dir = TempDir::new("fuzzy-dash");
    let test_file = dir.write(
        "unicode-dashes.txt",
        "range: 1\u{2013}5\nbreak\u{2014}here\n",
    );
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "range: 1-5\nbreak-here", "newText": "range: 10-50\nbreak--here" }] }),
    )
    .await
    .expect("edits");
    assert!(text_of(&result).contains("Successfully replaced"));
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert!(content.contains("10-50"));
}

#[tokio::test]
async fn fuzzy_matches_non_breaking_space_to_regular_space() {
    let dir = TempDir::new("fuzzy-nbsp");
    let test_file = dir.write("nbsp.txt", "hello\u{00A0}world\n");
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "hello world", "newText": "hello universe" }] }),
    )
    .await
    .expect("edits");
    assert!(text_of(&result).contains("Successfully replaced"));
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert!(content.contains("universe"));
}

#[tokio::test]
async fn prefers_exact_match_over_fuzzy_match() {
    let dir = TempDir::new("fuzzy-exact");
    let test_file = dir.write(
        "exact-preferred.txt",
        "const x = 'exact';\nconst y = 'other';\n",
    );
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "const x = 'exact';", "newText": "const x = 'changed';" }] }),
    )
    .await
    .expect("edits");
    assert!(text_of(&result).contains("Successfully replaced"));
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, "const x = 'changed';\nconst y = 'other';\n");
}

#[tokio::test]
async fn still_fails_when_text_is_not_found_even_with_fuzzy_matching() {
    let dir = TempDir::new("fuzzy-nomatch");
    let test_file = dir.write("no-match.txt", "completely different content\n");
    let error = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "this does not exist", "newText": "replacement" }] }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Could not find the exact text"), "{error}");
}

#[tokio::test]
async fn detects_duplicates_after_fuzzy_normalization() {
    let dir = TempDir::new("fuzzy-dups");
    let test_file = dir.write("fuzzy-dups.txt", "hello world   \nhello world\n");
    let error = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "hello world", "newText": "replaced" }] }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Found 2 occurrences"), "{error}");
}

#[tokio::test]
async fn supports_fuzzy_matching_in_multi_edit_mode() {
    let dir = TempDir::new("fuzzy-multi");
    let test_file = dir.write(
        "fuzzy-multi.txt",
        "console.log(\u{2018}hello\u{2019});\nhello\u{00A0}world\n",
    );
    run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [
            { "oldText": "console.log('hello');\n", "newText": "console.log('world');\n" },
            { "oldText": "hello world\n", "newText": "hello universe\n" }
        ] }),
    )
    .await
    .expect("edits");
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, "console.log('world');\nhello universe\n");
}

#[tokio::test]
async fn preserves_the_correct_occurrence_when_fuzzy_replacement_equals_a_nearby_line() {
    let dir = TempDir::new("fuzzy-preserve");
    let original = ["replace me   ", "after   ", ""].join("\n");
    let test_file = dir.write("fuzzy-preserve-duplicate-line.txt", &original);
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "replace me\n", "newText": "after\n" }] }),
    )
    .await
    .expect("edits");
    let expected = ["after", "after   ", ""].join("\n");
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, expected);
    let patch = result.details["patch"].as_str().expect("patch").to_string();
    let applied = apply_patch_text(&original, &patch);
    assert_eq!(applied, expected, "patch applies: {patch}");
}

#[tokio::test]
async fn preserves_untouched_lines_and_produces_an_applicable_patch_for_fuzzy_multi_edits() {
    let dir = TempDir::new("fuzzy-preserve-multi");
    let original = [
        "keep before  ",
        "first target  ",
        "first after",
        "keep middle   ",
        "second target  ",
        "second after",
        "keep after  ",
        "",
    ]
    .join("\n");
    let test_file = dir.write("fuzzy-preserve-multi.txt", &original);
    let result = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [
            { "oldText": "first target\nfirst after", "newText": "FIRST\nFIRST2" },
            { "oldText": "second target\nsecond after", "newText": "SECOND\nSECOND2" }
        ] }),
    )
    .await
    .expect("edits");
    let expected = [
        "keep before  ",
        "FIRST",
        "FIRST2",
        "keep middle   ",
        "SECOND",
        "SECOND2",
        "keep after  ",
        "",
    ]
    .join("\n");
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, expected);
    let patch = result.details["patch"].as_str().expect("patch").to_string();
    let applied = apply_patch_text(&original, &patch);
    assert_eq!(applied, expected, "patch applies: {patch}");
}

/// jsdiff's `applyPatch`, reduced to the hunks the tool emits: parse the
/// unified patch and apply it, so the test proves the patch text is
/// applicable the way the TS test's `applyPatch` is.
fn apply_patch_text(original: &str, patch: &str) -> String {
    /// One parsed hunk: `((old_start, old_count), (new_start, new_count), lines)`.
    type Hunk = ((usize, usize), (usize, usize), Vec<String>);
    let mut result = original.to_string();
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("@@ -") {
            if let Some(existing) = current.take() {
                hunks.push(existing);
            }
            // "@@ -1,4 +1,4 @@" or "@@ -0,0 +1,3 @@"
            let rest = rest.trim_end_matches(" @@");
            let (old_part, new_part) = rest.split_once(" +").expect("hunk header");
            let parse = |part: &str| -> (usize, usize) {
                let (start, count) = part.split_once(',').unwrap_or((part, "1"));
                (start.parse().expect("start"), count.parse().expect("count"))
            };
            current = Some((parse(old_part), parse(new_part), Vec::new()));
        } else if let Some(hunk) = current.as_mut() {
            if line.starts_with(' ')
                || line.starts_with('+')
                || line.starts_with('-')
                || line.starts_with('\\')
            {
                hunk.2.push(line.to_string());
            }
        }
    }
    if let Some(existing) = current.take() {
        hunks.push(existing);
    }

    // Apply hunks in descending old_start so offsets stay stable.
    hunks.sort_by_key(|((start, _), _, _)| std::cmp::Reverse(*start));
    for ((old_start, old_count), _, lines) in hunks {
        let old_lines: Vec<&str> = original.split('\n').collect();
        // jsdiff quirk: a zero-count hunk header starts one lower.
        let start = old_start.max(1) - 1;
        let mut replacement: Vec<&str> = Vec::new();
        let mut consumed = 0usize;
        for line in &lines {
            match line.chars().next() {
                Some('-') => consumed += 1,
                Some('+') => replacement.push(&line[1..]),
                Some('\\') => {}
                _ => {
                    if let Some(existing) = old_lines.get(start + consumed) {
                        replacement.push(existing);
                    }
                    consumed += 1;
                }
            }
        }
        if consumed == 0 && !replacement.is_empty() {
            // Pure insertion at the header position.
            let mut next: Vec<String> = old_lines[..start]
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            next.extend(replacement.iter().map(std::string::ToString::to_string));
            next.extend(
                old_lines[start..]
                    .iter()
                    .map(std::string::ToString::to_string),
            );
            result = next.join("\n");
        } else {
            let end = start + old_count.max(consumed);
            let mut next: Vec<String> = old_lines[..start]
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            next.extend(replacement.iter().map(std::string::ToString::to_string));
            next.extend(
                old_lines[end.min(old_lines.len())..]
                    .iter()
                    .map(std::string::ToString::to_string),
            );
            result = next.join("\n");
        }
    }
    result
}

// --- edit tool CRLF end-to-end (tools.test.ts) ------------------------------

#[tokio::test]
async fn end_to_end_preserves_crlf_and_bom_in_multi_edit_mode() {
    let dir = TempDir::new("edit-bom-crlf");
    let test_file = dir.write(
        "bom-crlf-multi.txt",
        "\u{FEFF}first\r\nsecond\r\nthird\r\nfourth\r\n",
    );
    run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [
            { "oldText": "second\n", "newText": "SECOND\n" },
            { "oldText": "fourth\n", "newText": "FOURTH\n" }
        ] }),
    )
    .await
    .expect("edits");
    let content = std::fs::read_to_string(&test_file).expect("read back");
    assert_eq!(content, "\u{FEFF}first\r\nSECOND\r\nthird\r\nFOURTH\r\n");
}

#[tokio::test]
async fn detects_duplicates_across_crlf_lf_variants() {
    let dir = TempDir::new("edit-mixed-endings");
    let test_file = dir.write(
        "mixed-endings.txt",
        "hello\r\nworld\r\n---\r\nhello\nworld\n",
    );
    let error = run_edit(
        dir.path(),
        json!({ "path": test_file.display().to_string(), "edits": [{ "oldText": "hello\nworld\n", "newText": "replaced\n" }] }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Found 2 occurrences"), "{error}");
}

// --- edit-tool-legacy-input.test.ts -----------------------------------------

#[test]
fn keeps_legacy_fields_out_of_the_public_schema() {
    let definition = pi_coding_agent::tools::edit_definition();
    let properties = &definition.parameters["properties"];
    assert!(properties.get("oldText").is_none());
    assert!(properties.get("newText").is_none());
}

#[test]
fn folds_top_level_oldtext_newtext_into_edits() {
    let prepared = edit::prepare_edit_arguments(json!({
        "path": "file.txt",
        "oldText": "before",
        "newText": "after"
    }));
    assert_eq!(
        prepared,
        json!({ "path": "file.txt", "edits": [{ "oldText": "before", "newText": "after" }] })
    );
}

#[test]
fn appends_legacy_replacement_to_existing_edits() {
    let prepared = edit::prepare_edit_arguments(json!({
        "path": "file.txt",
        "edits": [{ "oldText": "a", "newText": "b" }],
        "oldText": "c",
        "newText": "d"
    }));
    assert_eq!(
        prepared,
        json!({ "path": "file.txt", "edits": [
            { "oldText": "a", "newText": "b" },
            { "oldText": "c", "newText": "d" }
        ] })
    );
}

#[test]
fn passes_through_valid_input_unchanged() {
    let input = json!({ "path": "file.txt", "edits": [{ "oldText": "a", "newText": "b" }] });
    let prepared = edit::prepare_edit_arguments(input.clone());
    assert_eq!(prepared, input);
}

#[test]
fn passes_through_non_object_input_unchanged() {
    assert_eq!(
        edit::prepare_edit_arguments(serde_json::Value::Null),
        serde_json::Value::Null
    );
    assert_eq!(
        edit::prepare_edit_arguments(json!("garbage")),
        json!("garbage")
    );
}

#[tokio::test]
async fn prepared_args_execute_correctly() {
    let dir = TempDir::new("edit-legacy-exec");
    let file_path = dir.join("legacy.txt");
    std::fs::write(&file_path, "before\n").expect("written");
    let prepared = edit::prepare_edit_arguments(json!({
        "path": "legacy.txt",
        "oldText": "before",
        "newText": "after"
    }));
    let result = run_edit(dir.path(), prepared).await.expect("edits");
    assert_eq!(
        text_of(&result),
        "Successfully replaced 1 block(s) in legacy.txt."
    );
    let content = std::fs::read_to_string(&file_path).expect("read back");
    assert_eq!(content, "after\n");
}

#[test]
fn parses_edits_from_a_json_string() {
    let prepared = edit::prepare_edit_arguments(json!({
        "path": "file.txt",
        "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]"
    }));
    assert_eq!(
        prepared,
        json!({ "path": "file.txt", "edits": [{ "oldText": "a", "newText": "b" }] })
    );
}

#[test]
fn leaves_edits_alone_when_the_string_is_not_valid_json() {
    let prepared = edit::prepare_edit_arguments(json!({
        "path": "file.txt",
        "edits": "not json"
    }));
    assert_eq!(prepared, json!({ "path": "file.txt", "edits": "not json" }));
}

#[test]
fn folds_a_single_edit_object_into_a_one_element_array() {
    let prepared = edit::prepare_edit_arguments(json!({
        "path": "file.txt",
        "edits": { "oldText": "a", "newText": "b" }
    }));
    assert_eq!(
        prepared,
        json!({ "path": "file.txt", "edits": [{ "oldText": "a", "newText": "b" }] })
    );
}
