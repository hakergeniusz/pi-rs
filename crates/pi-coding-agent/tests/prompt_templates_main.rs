//! Port of `prompt-templates.test.ts`.

use pi_coding_agent::prompt_templates::{
    LoadPromptTemplatesOptions, PromptTemplate, Scope, expand_prompt_template,
    load_prompt_templates, parse_command_args, substitute_args,
};

/// String arguments, the shape `substitute_args` takes.
#[allow(dead_code)]
fn str_args(values: &[&str]) -> Vec<String> {
    values
        .iter()
        .map(std::string::ToString::to_string)
        .collect()
}

fn template(name: &str, content: &str) -> PromptTemplate {
    PromptTemplate {
        name: name.to_string(),
        description: "test".to_string(),
        argument_hint: None,
        content: content.to_string(),
        source: "local".to_string(),
        scope: Scope::Temporary,
        file_path: format!("/tmp/{name}.md").into(),
    }
}

// ============================================================================
// substituteArgs
// ============================================================================

#[test]
fn substitutes_all_args_placeholders() {
    assert_eq!(
        substitute_args("Test: $ARGUMENTS", &str_args(&["a", "b", "c"])),
        "Test: a b c"
    );
    assert_eq!(
        substitute_args("Test: $@", &str_args(&["a", "b", "c"])),
        "Test: a b c"
    );
    let args = vec!["foo".to_string(), "bar".to_string(), "baz".to_string()];
    assert_eq!(
        substitute_args("Test: $@", &args),
        substitute_args("Test: $ARGUMENTS", &args)
    );
}

// CRITICAL: argument values containing patterns should remain literal
#[test]
fn does_not_recursively_substitute_patterns_in_argument_values() {
    assert_eq!(
        substitute_args("$ARGUMENTS", &str_args(&["$1", "$ARGUMENTS"])),
        "$1 $ARGUMENTS"
    );
    assert_eq!(substitute_args("$@", &str_args(&["$100", "$1"])), "$100 $1");
    assert_eq!(
        substitute_args("$ARGUMENTS", &str_args(&["$100", "$1"])),
        "$100 $1"
    );
}

#[test]
fn supports_mixed_placeholders() {
    assert_eq!(
        substitute_args("$1: $ARGUMENTS", &str_args(&["prefix", "a", "b"])),
        "prefix: prefix a b"
    );
    assert_eq!(
        substitute_args("$1: $@", &str_args(&["prefix", "a", "b"])),
        "prefix: prefix a b"
    );
}

#[test]
fn handles_empty_arguments() {
    assert_eq!(
        substitute_args("Test: $ARGUMENTS", &str_args(&[])),
        "Test: "
    );
    assert_eq!(substitute_args("Test: $@", &str_args(&[])), "Test: ");
    assert_eq!(substitute_args("Test: $1", &str_args(&[])), "Test: ");
}

#[test]
fn handles_multiple_and_mixed_occurrences() {
    assert_eq!(
        substitute_args("$ARGUMENTS and $ARGUMENTS", &str_args(&["a", "b"])),
        "a b and a b"
    );
    assert_eq!(
        substitute_args("$@ and $@", &str_args(&["a", "b"])),
        "a b and a b"
    );
    assert_eq!(
        substitute_args("$@ and $ARGUMENTS", &str_args(&["a", "b"])),
        "a b and a b"
    );
}

#[test]
fn handles_special_characters_in_arguments() {
    assert_eq!(
        substitute_args("$1 $2: $ARGUMENTS", &str_args(&["arg100", "@user"])),
        "arg100 @user: arg100 @user"
    );
}

#[test]
fn handles_out_of_range_numbered_placeholders() {
    // Out-of-range placeholders become empty strings (preserving spaces from
    // the template).
    assert_eq!(
        substitute_args("$1 $2 $3 $4 $5", &str_args(&["a", "b"])),
        "a b   "
    );
}

#[test]
fn handles_unicode_and_newlines() {
    assert_eq!(
        substitute_args("$ARGUMENTS", &str_args(&["日本語", "\u{1F389}", "café"])),
        "日本語 \u{1F389} café"
    );
    assert_eq!(
        substitute_args("$1 $2", &str_args(&["line1\nline2", "tab\tthere"])),
        "line1\nline2 tab\tthere"
    );
}

#[test]
fn handles_consecutive_dollar_patterns() {
    assert_eq!(substitute_args("$1$2", &str_args(&["a", "b"])), "ab");
}

#[test]
fn handles_quoted_arguments_with_spaces() {
    assert_eq!(
        substitute_args("$ARGUMENTS", &str_args(&["first arg", "second arg"])),
        "first arg second arg"
    );
    assert_eq!(
        substitute_args("Test: $ARGUMENTS", &str_args(&["only"])),
        "Test: only"
    );
    assert_eq!(
        substitute_args("Test: $@", &str_args(&["only"])),
        "Test: only"
    );
}

#[test]
fn handles_zero_index() {
    assert_eq!(substitute_args("$0", &str_args(&["a", "b"])), "");
}

#[test]
fn handles_decimal_number_in_pattern() {
    assert_eq!(substitute_args("$1.5", &str_args(&["a"])), "a.5");
}

#[test]
fn handles_placeholders_as_part_of_words() {
    assert_eq!(
        substitute_args("pre$ARGUMENTS", &str_args(&["a", "b"])),
        "prea b"
    );
    assert_eq!(substitute_args("pre$@", &str_args(&["a", "b"])), "prea b");
    assert_eq!(
        substitute_args("Prefix $ARGUMENTS suffix", &str_args(&["ARGUMENTS"])),
        "Prefix ARGUMENTS suffix"
    );
}

#[test]
fn handles_non_matching_patterns() {
    assert_eq!(
        substitute_args("$A $$ $ $ARGS", &str_args(&["a"])),
        "$A $$ $ $ARGS"
    );
}

#[test]
fn is_case_sensitive() {
    assert_eq!(
        substitute_args("$arguments $Arguments $ARGUMENTS", &str_args(&["a", "b"])),
        "$arguments $Arguments a b"
    );
}

#[test]
fn handles_both_syntaxes_identically() {
    let args = vec!["x".to_string(), "y".to_string(), "z".to_string()];
    let result1 = substitute_args("$@ and $ARGUMENTS", &args);
    let result2 = substitute_args("$ARGUMENTS and $@", &args);
    assert_eq!(result1, result2);
    assert_eq!(result1, "x y z and x y z");
}

#[test]
fn handles_empty_arguments_in_middle_of_list() {
    assert_eq!(
        substitute_args("$ARGUMENTS", &str_args(&["a", "", "c"])),
        "a  c"
    );
}

#[test]
fn handles_trailing_and_leading_spaces_in_arguments() {
    assert_eq!(
        substitute_args("$ARGUMENTS", &str_args(&["  leading  ", "trailing  "])),
        "  leading   trailing  "
    );
}

#[test]
fn handles_numbered_placeholders_with_multiple_digits() {
    let args: Vec<String> = (0..15).map(|i| format!("val{i}")).collect();
    assert_eq!(substitute_args("$10 $12 $15", &args), "val9 val11 val14");
}

#[test]
fn handles_escaped_dollar_signs_literally() {
    // No escape mechanism exists — backslash is treated literally.
    assert_eq!(
        substitute_args("Price: \\$100", &str_args(&[])),
        "Price: \\"
    );
}

#[test]
fn handles_mixed_numbered_and_wildcard_placeholders() {
    assert_eq!(
        substitute_args(
            "$1: $@ ($ARGUMENTS)",
            &str_args(&["first", "second", "third"])
        ),
        "first: first second third (first second third)"
    );
}

#[test]
fn handles_templates_without_placeholders() {
    assert_eq!(
        substitute_args("Just plain text", &str_args(&["a", "b"])),
        "Just plain text"
    );
    assert_eq!(
        substitute_args("$1 $2 $@", &str_args(&["a", "b", "c"])),
        "a b a b c"
    );
}

// ============================================================================
// substituteArgs — positional defaults
// ============================================================================

#[test]
fn uses_default_when_positional_arg_is_missing() {
    assert_eq!(
        substitute_args("List exactly ${1:-7} next steps", &str_args(&[])),
        "List exactly 7 next steps"
    );
    assert_eq!(
        substitute_args("List exactly ${1:-7} next steps", &str_args(&["3"])),
        "List exactly 3 next steps"
    );
    // Default also applies when the arg is present but empty.
    assert_eq!(
        substitute_args("Mode: ${1:-brief}", &str_args(&[""])),
        "Mode: brief"
    );
}

#[test]
fn supports_defaults_for_all_arguments() {
    let template = "${@:-default}\n${ARGUMENTS:-default}";
    assert_eq!(
        substitute_args(template, &str_args(&[])),
        "default\ndefault"
    );
    assert_eq!(
        substitute_args(
            template,
            &str_args(&["This", "would", "be", "the", "arguments"])
        ),
        "This would be the arguments\nThis would be the arguments"
    );
}

#[test]
fn supports_multiple_positional_defaults() {
    assert_eq!(
        substitute_args("${1:-7} ${2:-brief}", &str_args(&[])),
        "7 brief"
    );
    assert_eq!(
        substitute_args("${1:-7} ${2:-brief}", &str_args(&["3"])),
        "3 brief"
    );
    assert_eq!(
        substitute_args("${1:-7} ${2:-brief}", &str_args(&["3", "verbose"])),
        "3 verbose"
    );
}

#[test]
fn does_not_recursively_substitute_in_defaults() {
    assert_eq!(
        substitute_args("${1:-7}", &str_args(&["$ARGUMENTS"])),
        "$ARGUMENTS"
    );
    assert_eq!(substitute_args("${1:-7}", &str_args(&["$1"])), "$1");
    assert_eq!(
        substitute_args("${1:-$ARGUMENTS}", &str_args(&["a", "b"])),
        "a"
    );
    assert_eq!(
        substitute_args("${3:-$ARGUMENTS}", &str_args(&["a", "b"])),
        "$ARGUMENTS"
    );
}

#[test]
fn supports_defaults_with_spaces_and_out_of_range() {
    assert_eq!(
        substitute_args("${1:-seven steps}", &str_args(&[])),
        "seven steps"
    );
    assert_eq!(
        substitute_args("${3:-fallback}", &str_args(&["a", "b"])),
        "fallback"
    );
}

#[test]
fn mixes_positional_defaults_with_existing_placeholders() {
    assert_eq!(
        substitute_args("$1 ${2:-x} $ARGUMENTS", &str_args(&["a"])),
        "a x a"
    );
}

// ============================================================================
// substituteArgs — array slicing (bash style)
// ============================================================================

#[test]
fn slices_from_index() {
    assert_eq!(
        substitute_args("${@:2}", &str_args(&["a", "b", "c", "d"])),
        "b c d"
    );
    assert_eq!(
        substitute_args("${@:1}", &str_args(&["a", "b", "c"])),
        "a b c"
    );
    assert_eq!(
        substitute_args("${@:3}", &str_args(&["a", "b", "c", "d"])),
        "c d"
    );
    // ${@:0} as all args.
    assert_eq!(
        substitute_args("${@:0}", &str_args(&["a", "b", "c"])),
        "a b c"
    );
}

#[test]
fn slices_with_length() {
    assert_eq!(
        substitute_args("${@:2:2}", &str_args(&["a", "b", "c", "d"])),
        "b c"
    );
    assert_eq!(
        substitute_args("${@:1:1}", &str_args(&["a", "b", "c"])),
        "a"
    );
    assert_eq!(
        substitute_args("${@:3:1}", &str_args(&["a", "b", "c", "d"])),
        "c"
    );
    assert_eq!(
        substitute_args("${@:2:3}", &str_args(&["a", "b", "c", "d", "e"])),
        "b c d"
    );
}

#[test]
fn handles_out_of_range_and_zero_length_slices() {
    assert_eq!(substitute_args("${@:99}", &str_args(&["a", "b"])), "");
    assert_eq!(substitute_args("${@:5}", &str_args(&["a", "b"])), "");
    assert_eq!(substitute_args("${@:10:5}", &str_args(&["a", "b"])), "");
    assert_eq!(substitute_args("${@:2:0}", &str_args(&["a", "b", "c"])), "");
    assert_eq!(substitute_args("${@:1:0}", &str_args(&["a", "b"])), "");
}

#[test]
fn handles_length_exceeding_array() {
    assert_eq!(
        substitute_args("${@:2:99}", &str_args(&["a", "b", "c"])),
        "b c"
    );
    assert_eq!(substitute_args("${@:1:10}", &str_args(&["a", "b"])), "a b");
}

#[test]
fn processes_slice_before_simple_at() {
    assert_eq!(
        substitute_args("${@:2} vs $@", &str_args(&["a", "b", "c"])),
        "b c vs a b c"
    );
    assert_eq!(
        substitute_args("First: ${@:1:1}, All: $@", &str_args(&["x", "y", "z"])),
        "First: x, All: x y z"
    );
}

#[test]
fn does_not_recursively_substitute_slice_patterns_in_args() {
    assert_eq!(
        substitute_args("${@:1}", &str_args(&["${@:2}", "test"])),
        "${@:2} test"
    );
    assert_eq!(
        substitute_args("${@:2}", &str_args(&["a", "${@:3}", "c"])),
        "${@:3} c"
    );
}

#[test]
fn mixes_slices_with_positional_args() {
    assert_eq!(
        substitute_args("$1: ${@:2}", &str_args(&["cmd", "arg1", "arg2"])),
        "cmd: arg1 arg2"
    );
    assert_eq!(
        substitute_args("$1 $2 ${@:3}", &str_args(&["a", "b", "c", "d"])),
        "a b c d"
    );
}

#[test]
fn handles_empty_and_single_arg_slices() {
    assert_eq!(substitute_args("${@:2}", &str_args(&[])), "");
    assert_eq!(substitute_args("${@:1}", &str_args(&[])), "");
    assert_eq!(substitute_args("${@:1}", &str_args(&["only"])), "only");
    assert_eq!(substitute_args("${@:2}", &str_args(&["only"])), "");
}

#[test]
fn handles_slices_in_middle_of_text_and_multiple_slices() {
    assert_eq!(
        substitute_args(
            "Process ${@:2} with $1",
            &str_args(&["tool", "file1", "file2"])
        ),
        "Process file1 file2 with tool"
    );
    assert_eq!(
        substitute_args("${@:1:1} and ${@:2}", &str_args(&["a", "b", "c"])),
        "a and b c"
    );
    assert_eq!(
        substitute_args(
            "${@:1:2} vs ${@:3:2}",
            &str_args(&["a", "b", "c", "d", "e"])
        ),
        "a b vs c d"
    );
}

#[test]
fn handles_quoted_arguments_and_special_characters_in_slices() {
    assert_eq!(
        substitute_args("${@:2}", &str_args(&["cmd", "first arg", "second arg"])),
        "first arg second arg"
    );
    assert_eq!(
        substitute_args("${@:2}", &str_args(&["cmd", "$100", "@user", "#tag"])),
        "$100 @user #tag"
    );
    assert_eq!(
        substitute_args("${@:1}", &str_args(&["日本語", "\u{1F389}", "café"])),
        "日本語 \u{1F389} café"
    );
}

#[test]
fn combines_positional_slice_and_wildcard_placeholders() {
    let template = "Run $1 on ${@:2:2}, then process $@";
    let args: Vec<String> = ["eslint", "file1.ts", "file2.ts", "file3.ts"]
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    assert_eq!(
        substitute_args(template, &args),
        "Run eslint on file1.ts file2.ts, then process eslint file1.ts file2.ts file3.ts"
    );
}

#[test]
fn handles_slice_with_no_spacing_and_large_lengths() {
    assert_eq!(
        substitute_args("prefix${@:2}suffix", &str_args(&["a", "b", "c"])),
        "prefixb csuffix"
    );
    let args: Vec<String> = (1..=10).map(|i| format!("arg{i}")).collect();
    assert_eq!(
        substitute_args("${@:5:100}", &args),
        "arg5 arg6 arg7 arg8 arg9 arg10"
    );
}

// ============================================================================
// parseCommandArgs
// ============================================================================

fn arguments_of(input: &str) -> Vec<String> {
    parse_command_args(input)
}

#[test]
fn parses_simple_space_separated_arguments() {
    assert_eq!(arguments_of("a b c"), ["a", "b", "c"]);
}

#[test]
fn parses_quoted_arguments_with_spaces() {
    assert_eq!(
        arguments_of("\"first arg\" second"),
        ["first arg", "second"]
    );
    assert_eq!(arguments_of("'first arg' second"), ["first arg", "second"]);
    assert_eq!(
        arguments_of("\"double\" 'single' \"double again\""),
        ["double", "single", "double again"]
    );
}

#[test]
fn handles_empty_string_and_extra_spaces() {
    assert_eq!(arguments_of(""), Vec::<String>::new());
    assert_eq!(arguments_of("a  b   c"), ["a", "b", "c"]);
    assert_eq!(arguments_of("a\tb\tc"), ["a", "b", "c"]);
}

#[test]
fn handles_quoted_empty_strings() {
    // Empty quotes keep their position as an empty argument (#10867).
    assert_eq!(arguments_of("\"\" \" \""), ["", " "]);
    assert_eq!(arguments_of("a '' c"), ["a", "", "c"]);
    assert_eq!(arguments_of("a \"\""), ["a", ""]);
}

#[test]
fn handles_arguments_with_special_characters_and_unicode() {
    assert_eq!(arguments_of("$100 @user #tag"), ["$100", "@user", "#tag"]);
    assert_eq!(
        arguments_of("日本語 \u{1F389} café"),
        ["日本語", "\u{1F389}", "café"]
    );
}

#[test]
fn handles_newlines_in_quoted_arguments() {
    assert_eq!(
        arguments_of("\"line1\nline2\" second"),
        ["line1\nline2", "second"]
    );
}

#[test]
fn treats_unquoted_newlines_as_separators() {
    assert_eq!(
        arguments_of("label-2\n\nHere is some description #2."),
        ["label-2", "Here", "is", "some", "description", "#2."]
    );
    assert_eq!(arguments_of("a\n\n\tb  c"), ["a", "b", "c"]);
}

#[test]
fn handles_escaped_quotes_inside_quoted_strings() {
    // A backslash escapes the enclosing quote character inside a quoted
    // argument (#11861).
    assert_eq!(arguments_of("\"quoted \\\"text\\\"\""), ["quoted \"text\""]);
    assert_eq!(arguments_of("'it\\'s here' next"), ["it's here", "next"]);
    // Other backslashes stay literal, so paths keep them.
    assert_eq!(
        arguments_of("\"C:\\dir\\file\" '\\\\server\\share'"),
        ["C:\\dir\\file", "\\\\server\\share"]
    );
    assert_eq!(arguments_of("\"it's\" 'a\\\"b'"), ["it's", "a\\\"b"]);
    // Outside quotes a backslash is literal.
    assert_eq!(arguments_of("say \\\"hi"), ["say", "\\\"hi"]);
}

#[test]
fn keeps_apostrophes_inside_words() {
    // Apostrophes in normal English must not open a quote (#11861).
    assert_eq!(
        arguments_of("why doesn't the user's cache work"),
        ["why", "doesn't", "the", "user's", "cache", "work"]
    );
    assert_eq!(
        arguments_of("why doesn't this work"),
        ["why", "doesn't", "this", "work"]
    );
}

#[test]
fn treats_an_unclosed_quote_as_literal_text() {
    // A quote without a closing match is kept as text (#11861).
    assert_eq!(arguments_of("'tis the season"), ["'tis", "the", "season"]);
    assert_eq!(
        arguments_of("say \"hello world"),
        ["say", "\"hello", "world"]
    );
    assert_eq!(arguments_of("a '"), ["a", "'"]);
    assert_eq!(arguments_of("\"abc\\\""), ["\"abc\\\""]);
    assert_eq!(arguments_of("\"it's\" 'x"), ["it's", "'x"]);
}

#[test]
fn handles_leading_and_trailing_spaces() {
    assert_eq!(arguments_of("a b c   "), ["a", "b", "c"]);
    assert_eq!(arguments_of("   a b c"), ["a", "b", "c"]);
}

// ============================================================================
// expandPromptTemplate integration
// ============================================================================

// #10867
#[test]
fn uses_positional_default_for_empty_quoted_argument_without_shifting_later_args() {
    let templates = vec![template("review", "Review $2. Focus: ${1:-correctness}.")];
    assert_eq!(
        expand_prompt_template("/review \"\" src/index.ts", &templates),
        "Review src/index.ts. Focus: correctness."
    );
    assert_eq!(
        expand_prompt_template("/review '' src/index.ts", &templates),
        "Review src/index.ts. Focus: correctness."
    );
}

#[test]
fn splits_template_arguments_on_unquoted_newlines() {
    let templates = vec![template("arg-test", "- arg1: $1\n- rest: ${@:2}")];
    assert_eq!(
        expand_prompt_template(
            "/arg-test label-2\n\nHere is some description #2.",
            &templates
        ),
        "- arg1: label-2\n- rest: Here is some description #2."
    );
}

#[test]
fn supports_template_command_separated_from_args_by_newline() {
    let templates = vec![template("arg-test", "arg1: $1")];
    assert_eq!(
        expand_prompt_template("/arg-test\nlabel-2", &templates),
        "arg1: label-2"
    );
}

#[test]
fn parses_and_substitutes_together() {
    let args = parse_command_args("Button \"onClick handler\" \"disabled support\"");
    let result = substitute_args("Create component $1 with features: $ARGUMENTS", &args);
    assert_eq!(
        result,
        "Create component Button with features: Button onClick handler disabled support"
    );
    // The README example.
    let result = substitute_args(
        "Create a React component named $1 with features: $ARGUMENTS",
        &args,
    );
    assert_eq!(
        result,
        "Create a React component named Button with features: Button onClick handler disabled support"
    );
    let args = parse_command_args("feature1 feature2 feature3");
    assert_eq!(
        substitute_args("Implement: $@", &args),
        substitute_args("Implement: $ARGUMENTS", &args)
    );
}

// ============================================================================
// loadPromptTemplates — argument-hint frontmatter and diagnostics
// ============================================================================

#[test]
fn parses_argument_hint_from_frontmatter() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(
        dir.path().join("pr.md"),
        "---\ndescription: Review PRs from URLs with structured issue and code analysis\nargument-hint: \"<PR-URL>\"\n---\nYou are given one or more GitHub PR URLs: $@",
    )
    .expect("template written");
    std::fs::write(
        dir.path().join("cl.md"),
        "---\ndescription: Audit changelog entries before release\n---\nAudit changelog entries for all commits since the last release.",
    )
    .expect("template written");
    std::fs::write(
        dir.path().join("empty-hint.md"),
        "---\ndescription: A command with empty hint\nargument-hint: \"\"\n---\nDo something",
    )
    .expect("template written");

    let result = load_prompt_templates(&LoadPromptTemplatesOptions {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().to_path_buf(),
        prompt_paths: vec![dir.path().to_path_buf()],
        include_defaults: false,
    });
    let pr = result
        .templates
        .iter()
        .find(|template| template.name == "pr")
        .expect("pr");
    assert_eq!(pr.argument_hint.as_deref(), Some("<PR-URL>"));
    assert_eq!(
        pr.description,
        "Review PRs from URLs with structured issue and code analysis"
    );
    let cl = result
        .templates
        .iter()
        .find(|template| template.name == "cl")
        .expect("cl");
    assert_eq!(cl.argument_hint, None);
    let empty_hint = result
        .templates
        .iter()
        .find(|template| template.name == "empty-hint")
        .expect("empty-hint");
    // Empty argument-hint stays undefined.
    assert_eq!(empty_hint.argument_hint, None);
}

#[test]
fn reports_invalid_yaml_frontmatter_and_keeps_valid_siblings() {
    // Regression test for #9354.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let invalid_prompt_path = dir.path().join("invalid.md");
    std::fs::write(
        &invalid_prompt_path,
        "---\ndescription: Broken: unquoted colon\n---\nDo something.\n",
    )
    .expect("invalid template");
    std::fs::write(dir.path().join("valid.md"), "Valid prompt content.").expect("valid template");

    let result = load_prompt_templates(&LoadPromptTemplatesOptions {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().to_path_buf(),
        prompt_paths: vec![dir.path().to_path_buf()],
        include_defaults: false,
    });
    let names: Vec<&str> = result
        .templates
        .iter()
        .map(|template| template.name.as_str())
        .collect();
    assert_eq!(names, vec!["valid"]);
    assert_eq!(result.diagnostics.len(), 1);
    let diagnostic = &result.diagnostics[0];
    assert!(matches!(
        diagnostic.kind,
        pi_coding_agent::skills::DiagnosticKind::Warning
    ));
    assert_eq!(diagnostic.path, invalid_prompt_path);
    assert!(
        diagnostic.message.contains("line 1, column 14"),
        "{}",
        diagnostic.message
    );
}

#[test]
fn expands_loaded_templates() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("greet.md"), "Hello $1, full: $ARGUMENTS").expect("template");
    let result = load_prompt_templates(&LoadPromptTemplatesOptions {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().to_path_buf(),
        prompt_paths: vec![dir.path().to_path_buf()],
        include_defaults: false,
    });
    assert_eq!(
        expand_prompt_template("/greet world one two", &result.templates),
        "Hello world, full: world one two"
    );
    // Non-templates pass through.
    assert_eq!(
        expand_prompt_template("plain text", &result.templates),
        "plain text"
    );
    assert_eq!(
        expand_prompt_template("/unknown command", &result.templates),
        "/unknown command"
    );
}
