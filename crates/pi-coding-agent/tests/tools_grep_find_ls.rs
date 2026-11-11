//! Ports of the grep, find, and ls tool tests from `tools.test.ts`.

mod common;

use common::{TempDir, text_of};
use pi_coding_agent::tools::{find, grep, ls};
use serde_json::json;

async fn run_grep(
    cwd: &std::path::Path,
    arguments: serde_json::Value,
) -> Result<pi_agent::types::AgentToolResult, String> {
    let context = pi_coding_agent::tools::ToolContext::new(cwd);
    grep::execute(&arguments, &context, &pi_agent::types::AbortSignal::new())
        .await
        .map_err(|error| error.0)
}

async fn run_find(
    cwd: &std::path::Path,
    arguments: serde_json::Value,
) -> Result<pi_agent::types::AgentToolResult, String> {
    let context = pi_coding_agent::tools::ToolContext::new(cwd);
    find::execute(&arguments, &context, &pi_agent::types::AbortSignal::new())
        .await
        .map_err(|error| error.0)
}

async fn run_ls(
    cwd: &std::path::Path,
    arguments: serde_json::Value,
) -> Result<pi_agent::types::AgentToolResult, String> {
    let context = pi_coding_agent::tools::ToolContext::new(cwd);
    ls::execute(&arguments, &context, &pi_agent::types::AbortSignal::new())
        .await
        .map_err(|error| error.0)
}

// --- grep tool --------------------------------------------------------------

#[tokio::test]
async fn grep_includes_filename_when_searching_a_single_file() {
    let dir = TempDir::new("grep-single");
    let test_file = dir.write("example.txt", "first line\nmatch line\nlast line");
    let result = run_grep(
        dir.path(),
        json!({ "pattern": "match", "path": test_file.display().to_string() }),
    )
    .await
    .expect("greps");
    assert!(
        text_of(&result).contains("example.txt:2: match line"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn grep_respects_global_limit_and_includes_context_lines() {
    let dir = TempDir::new("grep-context");
    let test_file = dir.write(
        "context.txt",
        "before\nmatch one\nafter\nmiddle\nmatch two\nafter two",
    );
    let result = run_grep(
        dir.path(),
        json!({ "pattern": "match", "path": test_file.display().to_string(), "limit": 1, "context": 1 }),
    )
    .await
    .expect("greps");
    let output = text_of(&result);
    assert!(output.contains("context.txt-1- before"), "{output}");
    assert!(output.contains("context.txt:2: match one"));
    assert!(output.contains("context.txt-3- after"));
    assert!(output.contains("[1 matches limit reached. Use limit=2 for more, or refine pattern]"));
    assert!(!output.contains("match two"));
}

// https://github.com/earendil-works/pi/issues/9276
#[tokio::test]
async fn grep_prints_overlapping_context_once_and_keeps_the_last_matchs_trailing_context() {
    let dir = TempDir::new("grep-rg-context");
    let test_file = dir.write(
        "rg-context.txt",
        "a\nmatch one\nb\nmatch two\nmatch three\nc\nmatch four",
    );
    let result = run_grep(
        dir.path(),
        json!({ "pattern": "match", "path": test_file.display().to_string(), "context": 1, "limit": 2 }),
    )
    .await
    .expect("greps");
    assert_eq!(
        text_of(&result).split("\n\n").next().expect("first block"),
        [
            "rg-context.txt-1- a",
            "rg-context.txt:2: match one",
            "rg-context.txt-3- b",
            "rg-context.txt:4: match two",
            // A match inside the last allowed match's trailing context is
            // shown as context.
            "rg-context.txt-5- match three",
        ]
        .join("\n")
    );
}

// https://github.com/earendil-works/pi/issues/9276
#[tokio::test]
async fn grep_keeps_counting_matches_past_the_byte_cap() {
    let dir = TempDir::new("grep-byte-cap");
    let line = format!("match {}", "x".repeat(400));
    let test_file = dir.write("byte-cap.txt", &vec![line.clone(); 200].join("\n"));
    let result = run_grep(
        dir.path(),
        json!({ "pattern": "match", "path": test_file.display().to_string(), "limit": 150 }),
    )
    .await
    .expect("greps");
    let output = text_of(&result);
    assert!(output.contains("150 matches limit reached"), "{output}");
    assert!(output.contains("50.0KB limit reached"));
    assert_eq!(result.details["matchLimitReached"], json!(150));
    assert_eq!(result.details["truncation"]["totalLines"], json!(150));
}

// https://github.com/earendil-works/pi/issues/9276
#[tokio::test]
async fn grep_reports_truncation_when_stored_output_ends_exactly_at_the_byte_cap() {
    let dir = TempDir::new("grep-cap-boundary");
    // 103 rows of 500 bytes plus one of 12 bytes, with newlines, join to
    // exactly 50KB; one more match follows.
    let mut rows: Vec<String> = vec!["x".repeat(490); 103];
    rows.push("x".repeat(7));
    rows.push("extra x".to_string());
    let test_file = dir.write("x", &rows.join("\n"));
    let result = run_grep(
        dir.path(),
        json!({ "pattern": "x", "path": test_file.display().to_string(), "limit": 1000 }),
    )
    .await
    .expect("greps");
    assert!(
        text_of(&result).contains("[50.0KB limit reached]"),
        "{}",
        text_of(&result)
    );
    assert_eq!(result.details["truncation"]["totalLines"], json!(105));
}

// https://github.com/earendil-works/pi/issues/9276
#[tokio::test]
async fn grep_accepts_a_huge_context_value() {
    let dir = TempDir::new("grep-huge-context");
    let test_file = dir.write("huge-context.txt", "before\nneedle\nafter");
    let result = run_grep(
        dir.path(),
        json!({ "pattern": "needle", "path": test_file.display().to_string(), "context": 1e30 }),
    )
    .await
    .expect("greps");
    assert_eq!(
        text_of(&result),
        [
            "huge-context.txt-1- before",
            "huge-context.txt:2: needle",
            "huge-context.txt-3- after"
        ]
        .join("\n")
    );
}

#[tokio::test]
async fn grep_keeps_a_lone_cr_inside_a_line_and_strips_crlf_terminators() {
    let dir = TempDir::new("grep-cr");
    let test_file = dir.write("mixed-cr.txt", "one\rtwo needle\r\nnext\r\n");
    let plain = run_grep(
        dir.path(),
        json!({ "pattern": "needle", "path": test_file.display().to_string() }),
    )
    .await
    .expect("greps");
    assert_eq!(text_of(&plain), "mixed-cr.txt:1: one\rtwo needle");

    let with_context = run_grep(
        dir.path(),
        json!({ "pattern": "needle", "path": test_file.display().to_string(), "context": 1 }),
    )
    .await
    .expect("greps");
    assert_eq!(
        text_of(&with_context),
        ["mixed-cr.txt:1: one\rtwo needle", "mixed-cr.txt-2- next"].join("\n")
    );
}

#[tokio::test]
async fn grep_does_not_report_the_match_limit_when_exactly_limit_matches_exist() {
    let dir = TempDir::new("grep-exact-limit");
    let test_file = dir.write("exact-limit.txt", "match one\nx\nmatch two\ny");
    for context in [0.0, 1.0] {
        let exact = run_grep(
            dir.path(),
            json!({ "pattern": "match", "path": test_file.display().to_string(), "limit": 2, "context": context }),
        )
        .await
        .expect("greps");
        assert!(!text_of(&exact).contains("limit reached"));
        assert!(
            exact
                .details
                .get("matchLimitReached")
                .is_none_or(serde_json::Value::is_null)
        );

        let over = run_grep(
            dir.path(),
            json!({ "pattern": "match", "path": test_file.display().to_string(), "limit": 1, "context": context }),
        )
        .await
        .expect("greps");
        assert!(
            text_of(&over)
                .contains("[1 matches limit reached. Use limit=2 for more, or refine pattern]")
        );
        assert_eq!(over.details["matchLimitReached"], json!(1));
    }

    let fractional = run_grep(
        dir.path(),
        json!({ "pattern": "match", "path": test_file.display().to_string(), "limit": 1.5 }),
    )
    .await
    .expect("greps");
    assert!(
        text_of(&fractional)
            .contains("[1 matches limit reached. Use limit=2 for more, or refine pattern]")
    );
}

// https://github.com/earendil-works/pi/issues/11203
#[tokio::test]
async fn grep_ignores_ripgrep_config_path() {
    let dir = TempDir::new("grep-config");
    let config_file = dir.join("rg-config");
    std::fs::write(&config_file, "--fixed-strings\n").expect("config written");
    let test_file = dir.write("fixture.txt", "aXb\na.b\n");
    // SAFETY-free variant: the env var is set for the rg child only by
    // passing it through the executor env, which rg's --no-config ignores.
    let mut context = pi_coding_agent::tools::ToolContext::new(dir.path());
    context.env.insert(
        "RIPGREP_CONFIG_PATH".to_string(),
        config_file.display().to_string(),
    );
    let result = grep::execute(
        &json!({ "pattern": "a.b", "path": test_file.display().to_string(), "literal": false }),
        &context,
        &pi_agent::types::AbortSignal::new(),
    )
    .await
    .expect("greps");
    let output = text_of(&result);
    assert!(output.contains("fixture.txt:1: aXb"), "{output}");
    assert!(output.contains("fixture.txt:2: a.b"));
}

// https://github.com/earendil-works/pi/issues/11807
#[tokio::test]
async fn grep_resolves_folder_globs_relative_to_path() {
    let dir = TempDir::new("grep-glob-root");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("migrations/main")).expect("dirs");
    std::fs::create_dir_all(repo.join("nested/migrations/main")).expect("dirs");
    std::fs::write(repo.join("migrations/main/a.js"), "hello\n").expect("a.js");
    std::fs::write(repo.join("nested/migrations/main/b.js"), "hello\n").expect("b.js");
    // rg canonicalizes its cwd, so a symlinked path must still anchor the glob.
    let link = dir.path().join("repo-link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&repo, &link).expect("symlink");

    let search_paths: Vec<std::path::PathBuf> = if link.exists() {
        vec![repo.clone(), link.clone()]
    } else {
        vec![repo.clone()]
    };
    for search_path in search_paths {
        let anchored = run_grep(
            dir.path(),
            json!({ "pattern": "hello", "path": search_path.display().to_string(), "glob": "migrations/main/*.js" }),
        )
        .await
        .expect("greps");
        // The decoy under nested/ stays out: the glob is anchored at path.
        assert_eq!(text_of(&anchored), "migrations/main/a.js:1: hello");

        let any_depth = run_grep(
            dir.path(),
            json!({ "pattern": "hello", "path": search_path.display().to_string(), "glob": "**/migrations/main/*.js" }),
        )
        .await
        .expect("greps");
        let any_depth_text = text_of(&any_depth);
        let mut lines: Vec<&str> = any_depth_text.split('\n').collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            vec![
                "migrations/main/a.js:1: hello",
                "nested/migrations/main/b.js:1: hello"
            ]
        );

        // rg matches nothing for a leading "./", so the tool drops it.
        let dot_slash = run_grep(
            dir.path(),
            json!({ "pattern": "hello", "path": search_path.display().to_string(), "glob": "./migrations/main/*.js" }),
        )
        .await
        .expect("greps");
        assert_eq!(text_of(&dot_slash), "migrations/main/a.js:1: hello");

        let negated = run_grep(
            dir.path(),
            json!({ "pattern": "hello", "path": search_path.display().to_string(), "glob": "!./migrations/**" }),
        )
        .await
        .expect("greps");
        assert_eq!(text_of(&negated), "nested/migrations/main/b.js:1: hello");
    }
}

#[tokio::test]
async fn grep_treats_flag_like_patterns_as_search_text() {
    let dir = TempDir::new("grep-injection");
    let marker = dir.join("grep-injection-marker");
    let payload = dir.join("payload.sh");
    let _test_file = dir.write("target.txt", "target\n");
    std::fs::write(
        &payload,
        format!(
            "#!/bin/sh\necho executed > {}\ncat \"$1\"\n",
            marker.display()
        ),
    )
    .expect("payload");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let result = run_grep(
        dir.path(),
        json!({ "pattern": format!("--pre={}", payload.display()), "path": dir.path().display().to_string() }),
    )
    .await
    .expect("greps");
    assert!(
        text_of(&result).contains("No matches found"),
        "{}",
        text_of(&result)
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn grep_reports_path_not_found() {
    let dir = TempDir::new("grep-missing");
    let error = run_grep(
        dir.path(),
        json!({ "pattern": "x", "path": dir.join("nope").display().to_string() }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Path not found"), "{error}");
}

// --- find tool --------------------------------------------------------------

#[tokio::test]
async fn find_includes_hidden_files_that_are_not_gitignored() {
    let dir = TempDir::new("find-hidden");
    std::fs::create_dir_all(dir.join(".secret")).expect("hidden dir");
    std::fs::write(dir.join(".secret/hidden.txt"), "hidden").expect("hidden file");
    std::fs::write(dir.join("visible.txt"), "visible").expect("visible file");
    let result = run_find(
        dir.path(),
        json!({ "pattern": "**/*.txt", "path": dir.path().display().to_string() }),
    )
    .await
    .expect("finds");
    let lines: Vec<String> = text_of(&result)
        .split('\n')
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    assert!(lines.contains(&"visible.txt".to_string()), "{lines:?}");
    assert!(
        lines.contains(&".secret/hidden.txt".to_string()),
        "{lines:?}"
    );
}

#[tokio::test]
async fn find_respects_gitignore() {
    let dir = TempDir::new("find-gitignore");
    std::fs::write(dir.join(".gitignore"), "ignored.txt\n").expect("gitignore");
    std::fs::write(dir.join("ignored.txt"), "ignored").expect("ignored");
    std::fs::write(dir.join("kept.txt"), "kept").expect("kept");
    let result = run_find(
        dir.path(),
        json!({ "pattern": "**/*.txt", "path": dir.path().display().to_string() }),
    )
    .await
    .expect("finds");
    let output = text_of(&result);
    assert!(output.contains("kept.txt"), "{output}");
    assert!(!output.contains("ignored.txt"), "{output}");
}

#[tokio::test]
async fn find_surfaces_fd_glob_parse_errors() {
    let dir = TempDir::new("find-bad-glob");
    let error = run_find(
        dir.path(),
        json!({ "pattern": "[", "path": dir.path().display().to_string() }),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_lowercase().contains("error parsing glob")
            || error.to_lowercase().contains("fd exited with code 1")
            || error.to_lowercase().contains("fd error"),
        "{error}"
    );
}

#[tokio::test]
async fn find_treats_flag_like_patterns_as_search_text() {
    let dir = TempDir::new("find-flag");
    let result = run_find(
        dir.path(),
        json!({ "pattern": "--help", "path": dir.path().display().to_string() }),
    )
    .await
    .expect("finds");
    assert!(text_of(&result).contains("No files found matching pattern"));
}

// #12020: fd matches nothing for a leading "./", so the tool drops it.
#[tokio::test]
async fn find_ignores_a_leading_dot_slash_in_the_pattern() {
    let dir = TempDir::new("find-dot-slash");
    std::fs::create_dir_all(dir.join("src")).expect("src");
    std::fs::write(dir.join("src/a.ts"), "a").expect("a.ts");
    std::fs::write(dir.join("package.json"), "{}").expect("package.json");
    for pattern in ["./src/*.ts", "./src/**/*.ts", "././src/*.ts"] {
        let result = run_find(
            dir.path(),
            json!({ "pattern": pattern, "path": dir.path().display().to_string() }),
        )
        .await
        .expect("finds");
        assert_eq!(text_of(&result), "src/a.ts", "pattern {pattern}");
    }
    // Without a "/" left, "./*.json" matches basenames at any depth, like
    // "*.json".
    std::fs::write(dir.join("src/b.json"), "{}").expect("b.json");
    let json = run_find(
        dir.path(),
        json!({ "pattern": "./*.json", "path": dir.path().display().to_string() }),
    )
    .await
    .expect("finds");
    let json_text = text_of(&json);
    let mut lines: Vec<&str> = json_text.split('\n').collect();
    lines.sort_unstable();
    assert_eq!(lines, vec!["package.json", "src/b.json"]);
}

// #7121: exactly `limit` matches must not report that the limit was reached.
#[tokio::test]
async fn find_reports_the_result_limit_only_when_more_results_exist() {
    let dir = TempDir::new("find-limit");
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), name).expect("file");
    }
    let exact = run_find(
        dir.path(),
        json!({ "pattern": "*.txt", "path": dir.path().display().to_string(), "limit": 3 }),
    )
    .await
    .expect("finds");
    let exact_text = text_of(&exact);
    let mut lines: Vec<&str> = exact_text.split('\n').collect();
    lines.sort_unstable();
    assert_eq!(lines, vec!["a.txt", "b.txt", "c.txt"]);
    assert!(
        exact
            .details
            .get("resultLimitReached")
            .is_none_or(serde_json::Value::is_null)
    );

    let over = run_find(
        dir.path(),
        json!({ "pattern": "*.txt", "path": dir.path().display().to_string(), "limit": 2 }),
    )
    .await
    .expect("finds");
    let over_text = text_of(&over);
    let parts: Vec<&str> = over_text.split("\n\n").collect();
    assert_eq!(parts[0].split('\n').count(), 2);
    assert_eq!(
        parts[1],
        "[2 results limit reached. Use limit=4 for more, or refine pattern]"
    );
    assert_eq!(over.details["resultLimitReached"], json!(2));
}

#[tokio::test]
async fn find_treats_limits_below_1_as_1_and_floors_fractional_limits() {
    let dir = TempDir::new("find-small-limit");
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), name).expect("file");
    }
    for limit in [0.0, -5.0, 0.5, 1.9] {
        let result = run_find(
            dir.path(),
            json!({ "pattern": "*.txt", "path": dir.path().display().to_string(), "limit": limit }),
        )
        .await
        .expect("finds");
        let result_text = text_of(&result);
        let parts: Vec<&str> = result_text.split("\n\n").collect();
        assert_eq!(parts[0].split('\n').count(), 1, "limit {limit}");
        assert_eq!(
            parts[1],
            "[1 results limit reached. Use limit=2 for more, or refine pattern]"
        );
        assert_eq!(result.details["resultLimitReached"], json!(1));
    }
}

#[tokio::test]
async fn find_reports_full_output_totals_past_the_byte_cap() {
    // 300 names of 200 bytes join to about 60KB; equal lengths make the kept
    // prefix independent of fd's order.
    let dir = TempDir::new("find-byte-cap");
    let names: Vec<String> = (0..300)
        .map(|i| format!("{:03}{}.txt", i, "x".repeat(193)))
        .collect();
    for name in &names {
        std::fs::write(dir.join(name), "").expect("file");
    }
    let result = run_find(
        dir.path(),
        json!({ "pattern": "*.txt", "path": dir.path().display().to_string() }),
    )
    .await
    .expect("finds");
    let expected = pi_coding_agent::tools::truncate::truncate_head(
        &names.join("\n"),
        pi_coding_agent::tools::truncate::TruncationOptions {
            max_lines: Some(usize::MAX),
            ..pi_coding_agent::tools::truncate::TruncationOptions::default()
        },
    );
    assert_eq!(
        result.details["truncation"]["totalLines"],
        json!(expected.total_lines)
    );
    assert_eq!(
        result.details["truncation"]["totalBytes"],
        json!(expected.total_bytes)
    );
    assert_eq!(result.details["truncation"]["truncatedBy"], json!("bytes"));
    assert_eq!(
        result.details["truncation"]["outputLines"],
        json!(expected.output_lines)
    );
    assert!(
        result
            .details
            .get("resultLimitReached")
            .is_none_or(serde_json::Value::is_null)
    );
    assert!(
        text_of(&result).ends_with("\n\n[50.0KB limit reached]"),
        "{}",
        text_of(&result)
    );
}

// --- ls tool ----------------------------------------------------------------

#[tokio::test]
async fn ls_lists_dotfiles_and_directories() {
    let dir = TempDir::new("ls-dotfiles");
    std::fs::write(dir.join(".hidden-file"), "secret").expect("file");
    std::fs::create_dir_all(dir.join(".hidden-dir")).expect("dir");
    let result = run_ls(
        dir.path(),
        json!({ "path": dir.path().display().to_string() }),
    )
    .await
    .expect("lists");
    let output = text_of(&result);
    assert!(output.contains(".hidden-file"), "{output}");
    assert!(output.contains(".hidden-dir/"));
}

#[tokio::test]
async fn ls_treats_limits_below_1_as_1_and_floors_fractional_limits() {
    let dir = TempDir::new("ls-small-limit");
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), name).expect("file");
    }
    for limit in [0.0, -5.0, 0.5, 1.9] {
        let result = run_ls(
            dir.path(),
            json!({ "path": dir.path().display().to_string(), "limit": limit }),
        )
        .await
        .expect("lists");
        assert_eq!(
            text_of(&result),
            "a.txt\n\n[1 entries limit reached. Use limit=2 for more]",
            "limit {limit}"
        );
        assert_eq!(result.details["entryLimitReached"], json!(1));
    }
}

#[tokio::test]
async fn ls_reports_the_entry_limit_only_when_more_entries_exist() {
    let dir = TempDir::new("ls-exact-limit");
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), name).expect("file");
    }
    let result = run_ls(
        dir.path(),
        json!({ "path": dir.path().display().to_string(), "limit": 3 }),
    )
    .await
    .expect("lists");
    assert_eq!(text_of(&result), "a.txt\nb.txt\nc.txt");
    assert!(
        result
            .details
            .get("entryLimitReached")
            .is_none_or(serde_json::Value::is_null)
    );
}

#[tokio::test]
async fn ls_reports_path_not_found_and_not_a_directory() {
    let dir = TempDir::new("ls-errors");
    let error = run_ls(
        dir.path(),
        json!({ "path": dir.join("nope").display().to_string() }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("Path not found"), "{error}");
    let file = dir.write("file.txt", "x");
    let error = run_ls(dir.path(), json!({ "path": file.display().to_string() }))
        .await
        .unwrap_err();
    assert!(error.contains("Not a directory"), "{error}");
}
