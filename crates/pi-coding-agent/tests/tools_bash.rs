//! Ports of the bash tool tests from `tools.test.ts`. Every command runs in
//! a tempdir; nothing outside it is touched and nothing needs the network.

mod common;

use common::{TempDir, text_of};
use pi_coding_agent::tools::ToolContext;
use pi_coding_agent::tools::bash;
use serde_json::json;

async fn run_bash(
    cwd: &std::path::Path,
    command: &str,
) -> Result<pi_agent::types::AgentToolResult, String> {
    run_bash_with(cwd, json!({ "command": command })).await
}

async fn run_bash_with(
    cwd: &std::path::Path,
    arguments: serde_json::Value,
) -> Result<pi_agent::types::AgentToolResult, String> {
    let mut context = ToolContext::new(cwd);
    context.expose_session_environment = false;
    bash::execute(
        &arguments,
        &context,
        &pi_agent::types::AbortSignal::new(),
        None,
    )
    .await
    .map_err(|error| error.0)
}

#[tokio::test]
async fn executes_simple_commands() {
    let dir = TempDir::new("bash");
    let result = run_bash(dir.path(), "echo 'test output'")
        .await
        .expect("runs");
    assert!(text_of(&result).contains("test output"));
}

#[tokio::test]
async fn reports_non_zero_exit_codes_as_error_results_with_structured_content() {
    let dir = TempDir::new("bash-exit");
    let result = run_bash(dir.path(), "echo out; exit 3")
        .await
        .expect("runs");
    assert!(result.is_error);
    assert_eq!(text_of(&result), "out\n\n\nCommand exited with code 3");
    let structured = result.structured_content.expect("structured");
    assert_eq!(structured["output"], json!("out\n"));
    assert_eq!(structured["truncated"], json!(false));
    assert_eq!(structured["exit_code"], json!(3));

    let ok = run_bash(dir.path(), "echo fine").await.expect("runs");
    assert!(!ok.is_error);
    let structured = ok.structured_content.expect("structured");
    assert_eq!(structured["output"], json!("fine\n"));
    assert_eq!(structured["exit_code"], json!(0));

    let empty = run_bash(dir.path(), "true").await.expect("runs");
    assert_eq!(text_of(&empty), "(no output)");
    let structured = empty.structured_content.expect("structured");
    assert_eq!(structured["output"], json!(""));
    assert_eq!(structured["truncated"], json!(false));
}

#[tokio::test]
async fn returns_up_to_1_mib_of_output_in_structured_content() {
    // 3000 lines exceed the model-facing 2000 line limit but not 1 MiB.
    let dir = TempDir::new("bash-structured");
    let medium = run_bash(dir.path(), "seq 1 3000").await.expect("runs");
    assert!(!text_of(&medium).contains("\n1\n2\n"));
    assert_eq!(medium.details["truncation"]["truncated"], json!(true));
    let structured = medium.structured_content.expect("structured");
    assert_eq!(structured["truncated"], json!(false));
    let expected: Vec<String> = (1..=3000).map(|i| i.to_string()).collect();
    assert_eq!(
        structured["output"],
        json!(format!("{}\n", expected.join("\n")))
    );

    // About 2 MB: keeps the first and last 512 KiB around an omission marker.
    let large = run_bash(dir.path(), "seq 1 300000").await.expect("runs");
    let structured = large.structured_content.expect("structured");
    assert_eq!(structured["truncated"], json!(true));
    let output = structured["output"]
        .as_str()
        .expect("output string")
        .to_string();
    assert!(output.starts_with("1\n2\n3\n"), "{}", &output[..40]);
    assert!(
        output.ends_with("299999\n300000\n"),
        "{}",
        &output[output.len() - 40..]
    );
    assert!(output.contains("\n\n[... ") && output.contains(" bytes omitted ...]\n\n"));
    assert!(output.len() < 1024 * 1024 + 100);
    let full_output_path = structured["full_output_path"].as_str().expect("full path");
    assert_eq!(
        Some(full_output_path),
        large.details["fullOutputPath"].as_str()
    );
    let full = std::fs::read_to_string(full_output_path).expect("full output readable");
    assert!(full.ends_with("300000\n"));
}

// Regression tests for https://github.com/earendil-works/pi/issues/9577
#[tokio::test]
async fn maps_signal_killed_commands_to_128_plus_the_signal_number() {
    let dir = TempDir::new("bash-signal");
    let result = run_bash(dir.path(), "printf 'before-kill\\n'; kill -TERM $$")
        .await
        .expect("runs");
    assert!(result.is_error);
    let output = text_of(&result);
    assert!(
        output.ends_with("before-kill\n\n\nCommand exited with code 143")
            || output.ends_with("Command exited with code 143"),
        "{output}"
    );
    assert!(output.contains("before-kill"));
    assert!(output.contains("Command exited with code 143"));
}

#[tokio::test]
async fn respects_timeout() {
    let dir = TempDir::new("bash-timeout");
    let error = run_bash_with(
        dir.path(),
        json!({ "command": "sleep 30", "timeout": 0.05 }),
    )
    .await
    .unwrap_err();
    assert!(error.to_lowercase().contains("timed out"), "{error}");
    // The killed process leaves nothing running behind it.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[tokio::test]
async fn includes_full_output_path_for_timeout_errors() {
    let dir = TempDir::new("bash-timeout-full");
    let error = run_bash_with(
        dir.path(),
        json!({ "command": "seq 3000; sleep 30", "timeout": 0.4 }),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("Command timed out after 0.4 seconds"),
        "{error}"
    );
    assert!(error.contains("[Showing lines "), "{error}");
    assert!(!error.contains("Full output: undefined"), "{error}");
    let start = error.find("Full output: ").expect("marker") + "Full output: ".len();
    let end = error[start..]
        .find(']')
        .map_or(error.len(), |relative| start + relative);
    let full_output_path = &error[start..end];
    assert!(
        std::path::Path::new(full_output_path).exists(),
        "{full_output_path}"
    );
    let full = std::fs::read_to_string(full_output_path).expect("readable");
    assert!(full.contains("1\n2\n3"));
    assert!(full.contains("2998\n2999\n3000"));
}

#[tokio::test]
async fn throws_error_when_cwd_does_not_exist() {
    let nonexistent = "/this/directory/definitely/does/not/exist/12345";
    let error = run_bash(std::path::Path::new(nonexistent), "echo test")
        .await
        .unwrap_err();
    assert!(
        error.contains("Working directory does not exist"),
        "{error}"
    );
}

#[tokio::test]
async fn handles_process_spawn_errors() {
    // pi's spawn-failure path: a configured shell that exists but cannot be
    // executed surfaces the exec error (`bash.ts:650-659` mocks the shell to
    // a missing binary, which the shell-path check turns into "not found").
    let dir = TempDir::new("bash-bad-shell");
    let not_executable = dir.join("not-a-shell");
    std::fs::write(&not_executable, "not a shell").expect("written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
    }
    let mut context = ToolContext::new(dir.path());
    context.shell_path = Some(not_executable.display().to_string());
    let error = bash::execute(
        &json!({ "command": "echo test" }),
        &context,
        &pi_agent::types::AbortSignal::new(),
        None,
    )
    .await
    .unwrap_err();
    assert!(error.0.contains("Failed to run"), "{error}");
}

#[tokio::test]
async fn prepends_command_prefix_when_configured() {
    let dir = TempDir::new("bash-prefix");
    let mut context = ToolContext::new(dir.path());
    context.command_prefix = Some("export TEST_VAR=hello".to_string());
    let result = bash::execute(
        &json!({ "command": "echo $TEST_VAR" }),
        &context,
        &pi_agent::types::AbortSignal::new(),
        None,
    )
    .await
    .expect("runs");
    assert_eq!(text_of(&result).trim(), "hello");
}

#[tokio::test]
async fn includes_output_from_both_prefix_and_command() {
    let dir = TempDir::new("bash-prefix-both");
    let mut context = ToolContext::new(dir.path());
    context.command_prefix = Some("echo prefix-output".to_string());
    let result = bash::execute(
        &json!({ "command": "echo command-output" }),
        &context,
        &pi_agent::types::AbortSignal::new(),
        None,
    )
    .await
    .expect("runs");
    assert_eq!(text_of(&result).trim(), "prefix-output\ncommand-output");
}

#[tokio::test]
async fn works_without_command_prefix() {
    let dir = TempDir::new("bash-no-prefix");
    let result = run_bash(dir.path(), "echo no-prefix").await.expect("runs");
    assert_eq!(text_of(&result).trim(), "no-prefix");
}

#[tokio::test]
async fn coalesces_streaming_updates_for_chatty_output() {
    let dir = TempDir::new("bash-updates");
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut context = ToolContext::new(dir.path());
    context.expose_session_environment = false;
    let result = bash::execute(
        &json!({ "command": "seq 5000" }),
        &context,
        &pi_agent::types::AbortSignal::new(),
        Some(&sender),
    )
    .await
    .expect("runs");
    drop(sender);
    let mut updates = 0usize;
    while receiver.try_recv().is_ok() {
        updates += 1;
    }
    // pi throttles to one update per BASH_UPDATE_THROTTLE_MS plus the initial
    // empty update and the final snapshot.
    assert!(updates < 200, "updates: {updates}");
    assert!(text_of(&result).contains("5000"));
}

#[tokio::test]
async fn does_not_count_a_trailing_newline_as_an_extra_truncated_bash_output_line() {
    let dir = TempDir::new("bash-trailing");
    let result = run_bash(
        dir.path(),
        r#"awk 'BEGIN{for(i=1;i<=4000;i++) printf "line-%04d\n", i}'"#,
    )
    .await
    .expect("runs");
    let output = text_of(&result);
    assert_eq!(
        result.details["truncation"]["totalLines"],
        json!(4000),
        "{output}"
    );
    assert_eq!(result.details["truncation"]["outputLines"], json!(2000));
    assert!(output.contains("line-2001"));
    assert!(output.contains("line-4000"));
    assert!(output.contains("[Showing lines 2001-4000 of 4000. Full output: "));
    assert!(!output.contains("line-4001"));
}

#[tokio::test]
async fn decodes_utf8_characters_split_across_output_chunks() {
    // The euro sign is 3 bytes; the accumulator must reassemble it however the
    // scheduler splits the pipe reads.
    let dir = TempDir::new("bash-utf8");
    let result = run_bash(dir.path(), "printf '\\xe2\\x82\\xac\\n'")
        .await
        .expect("runs");
    assert_eq!(text_of(&result).trim(), "\u{20ac}");
}

#[tokio::test]
async fn persists_full_output_when_truncation_happens_by_line_count_only() {
    let dir = TempDir::new("bash-line-truncation");
    let result = run_bash(dir.path(), "seq 3000").await.expect("runs");
    let output = text_of(&result);
    assert_eq!(result.details["truncation"]["truncated"], json!(true));
    assert_eq!(result.details["truncation"]["truncatedBy"], json!("lines"));
    let full_output_path = result.details["fullOutputPath"]
        .as_str()
        .expect("full path");
    assert!(output.contains("[Showing lines "));
    assert!(!output.contains("Full output: undefined"));
    assert!(
        std::path::Path::new(full_output_path).exists(),
        "{full_output_path}"
    );
    let full = std::fs::read_to_string(full_output_path).expect("readable");
    assert!(full.contains("1\n2\n3"));
    assert!(full.contains("2998\n2999\n3000"));
}

#[tokio::test]
async fn exposes_pi_session_environment_when_enabled() {
    let dir = TempDir::new("bash-session-env");
    let mut context = ToolContext::new(dir.path());
    context.expose_session_environment = true;
    context
        .session_env
        .insert("PI_SESSION_ID".to_string(), "test-session".to_string());
    let result = bash::execute(
        &json!({ "command": "echo $PI_SESSION_ID" }),
        &context,
        &pi_agent::types::AbortSignal::new(),
        None,
    )
    .await
    .expect("runs");
    assert_eq!(text_of(&result).trim(), "test-session");

    // Off by the tool option, the variable is scrubbed from the child env.
    let mut hidden = ToolContext::new(dir.path());
    hidden.expose_session_environment = false;
    hidden
        .session_env
        .insert("PI_SESSION_ID".to_string(), "test-session".to_string());
    let result = bash::execute(
        &json!({ "command": "echo \"${PI_SESSION_ID:-unset}\"" }),
        &hidden,
        &pi_agent::types::AbortSignal::new(),
        None,
    )
    .await
    .expect("runs");
    assert_eq!(text_of(&result).trim(), "unset");
}

#[tokio::test]
async fn abort_kills_the_running_command() {
    let dir = TempDir::new("bash-abort");
    let context = ToolContext::new(dir.path());
    let signal = pi_agent::types::AbortSignal::new();
    let abort_signal = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        abort_signal.abort();
    });
    let started = std::time::Instant::now();
    let error = bash::execute(&json!({ "command": "sleep 30" }), &context, &signal, None)
        .await
        .unwrap_err();
    assert!(error.0.contains("Command aborted"), "{error}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "abort ends the command"
    );
}
