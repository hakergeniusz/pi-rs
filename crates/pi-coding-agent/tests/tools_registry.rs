//! Ports of the tool-seam tests: the JSON schemas, the executor seam
//! (`tool-definition-wrapper.ts`), `path-utils.test.ts`, and the tool cwd
//! resolution tests from `tools.test.ts`.

mod common;

use common::{TempDir, text_of};
use pi_agent::types::ToolSpec;
use pi_coding_agent::tools::{BuiltinToolExecutor, ToolRegistry, all_tool_definitions};
use serde_json::json;

fn executor_for(cwd: &std::path::Path) -> BuiltinToolExecutor {
    BuiltinToolExecutor::new(
        ToolRegistry::all(),
        pi_coding_agent::tools::ToolContext::new(cwd),
    )
}

#[test]
fn tool_names_match_pi() {
    // `allToolNames` (`index.ts:96-105`) minus the Windows-only powershell
    // variant, which is recorded as unsupported instead.
    let registry = ToolRegistry::all();
    assert_eq!(
        registry.names(),
        vec!["read", "bash", "edit", "write", "grep", "find", "ls"]
    );
    assert_eq!(pi_coding_agent::tools::TOOL_NAMES.len(), 8);
    assert_eq!(pi_coding_agent::tools::UNSUPPORTED_TOOLS[0].0, "powershell");
}

#[test]
fn read_schema_matches_the_typebox_shape() {
    let definition = all_tool_definitions()
        .into_iter()
        .find(|definition| definition.name == "read")
        .expect("read");
    let parameters = &definition.parameters;
    assert_eq!(parameters["type"], json!("object"));
    assert_eq!(parameters["required"], json!(["path"]));
    assert_eq!(parameters["properties"]["path"]["type"], json!("string"));
    assert_eq!(
        parameters["properties"]["path"]["description"],
        json!("Path to the file to read (relative or absolute)")
    );
    assert_eq!(
        parameters["properties"]["offset"]["description"],
        json!("Line number to start reading from (1-indexed)")
    );
    assert_eq!(
        parameters["properties"]["limit"]["description"],
        json!("Maximum number of lines to read")
    );
}

#[test]
fn edit_schema_keeps_legacy_fields_out_of_the_public_shape() {
    let definition = all_tool_definitions()
        .into_iter()
        .find(|definition| definition.name == "edit")
        .expect("edit");
    assert_eq!(definition.parameters["required"], json!(["path", "edits"]));
    assert!(definition.parameters["properties"].get("oldText").is_none());
    assert!(definition.parameters["properties"].get("newText").is_none());
    assert_eq!(
        definition.parameters["properties"]["edits"]["type"],
        json!("array")
    );
    assert_eq!(
        definition.parameters["properties"]["edits"]["items"]["required"],
        json!(["oldText", "newText"])
    );
}

#[test]
fn every_tool_has_the_right_prompt_contribution() {
    let definitions = all_tool_definitions();
    let read = definitions
        .iter()
        .find(|definition| definition.name == "read")
        .expect("read");
    assert_eq!(read.prompt_snippet, Some("Read file contents"));
    assert_eq!(
        read.prompt_guidelines,
        vec!["Use read to examine files instead of cat or sed."]
    );
    let bash = definitions
        .iter()
        .find(|definition| definition.name == "bash")
        .expect("bash");
    assert_eq!(
        bash.prompt_guidelines,
        vec!["You can inspect PI_* environment variables for current model and session details."]
    );
    // read/bash/edit/write declare constrained sampling; grep/find/ls do not.
    for definition in &definitions {
        let strict = matches!(
            definition.constrained_sampling,
            Some(pi_agent::types::ConstrainedSampling::JsonSchema {
                strict: pi_agent::types::StrictMode::Prefer
            })
        );
        assert_eq!(
            strict,
            matches!(definition.name, "read" | "bash" | "edit" | "write"),
            "{}",
            definition.name
        );
    }
}

#[test]
fn tool_specs_carry_the_declaration_the_loop_needs() {
    let registry = ToolRegistry::coding();
    let specs: Vec<ToolSpec> = registry.tool_specs();
    assert_eq!(
        specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        vec!["read", "bash", "edit", "write"]
    );
    for spec in &specs {
        assert_ne!(spec.description, "");
        assert!(spec.parameters.is_object());
    }
}

#[tokio::test]
async fn executor_runs_validation_and_the_tool_body() {
    let dir = TempDir::new("registry");
    let file = dir.write("test.txt", "hello");
    let executor = executor_for(dir.path());
    let result = executor
        .run(
            "read",
            &json!({ "path": file.display().to_string() }),
            &pi_agent::types::AbortSignal::new(),
            None,
        )
        .await
        .expect("reads");
    assert_eq!(text_of(&result), "hello");
    let error = executor
        .run(
            "read",
            &json!({ "offset": 1 }),
            &pi_agent::types::AbortSignal::new(),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        error.contains("Validation failed for tool \"read\""),
        "{error}"
    );
    assert!(
        error.contains("Expected required property 'path'"),
        "{error}"
    );
}

#[tokio::test]
async fn executor_coerces_string_arguments_like_pi() {
    let dir = TempDir::new("registry-coerce");
    let lines: Vec<String> = (0..100).map(|i| format!("Line {}", i + 1)).collect();
    let file = dir.write("limit-test.txt", &lines.join("\n"));
    let executor = executor_for(dir.path());
    // `"10"` coerces to the number 10.
    let result = executor
        .run(
            "read",
            &json!({ "path": file.display().to_string(), "limit": "10" }),
            &pi_agent::types::AbortSignal::new(),
            None,
        )
        .await
        .expect("reads");
    assert!(text_of(&result).contains("Line 10"));
    assert!(!text_of(&result).contains("Line 11"));
}

#[tokio::test]
async fn executor_unknown_tool_fails() {
    let dir = TempDir::new("registry-unknown");
    let executor = executor_for(dir.path());
    let error = executor
        .run(
            "does-not-exist",
            &json!({}),
            &pi_agent::types::AbortSignal::new(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error, "Tool does-not-exist not found");
}

// --- tool cwd resolution (tools.test.ts) ------------------------------------

#[tokio::test]
async fn every_tool_uses_the_executor_cwd() {
    let dir = TempDir::new("cwd-resolution");
    let executor = executor_for(dir.path());
    let abort = pi_agent::types::AbortSignal::new();

    let file = dir.write("ctx-cwd-read.txt", "hello from ctx.cwd");
    let result = executor
        .run(
            "read",
            &json!({ "path": file.display().to_string() }),
            &abort,
            None,
        )
        .await
        .expect("read");
    assert!(text_of(&result).contains("hello from ctx.cwd"));

    executor
        .run(
            "write",
            &json!({ "path": "ctx-cwd-write.txt", "content": "written via cwd" }),
            &abort,
            None,
        )
        .await
        .expect("write");
    let content = std::fs::read_to_string(dir.join("ctx-cwd-write.txt")).expect("read back");
    assert_eq!(content, "written via cwd");

    dir.write("ctx-cwd-edit.txt", "old text");
    executor
        .run(
            "edit",
            &json!({ "path": "ctx-cwd-edit.txt", "edits": [{ "oldText": "old text", "newText": "new text" }] }),
            &abort,
            None,
        )
        .await
        .expect("edit");
    let content = std::fs::read_to_string(dir.join("ctx-cwd-edit.txt")).expect("read back");
    assert_eq!(content, "new text");

    dir.write("ctx-cwd-grep.txt", "match in ctx.cwd");
    let result = executor
        .run("grep", &json!({ "pattern": "match" }), &abort, None)
        .await
        .expect("grep");
    assert!(text_of(&result).contains("ctx-cwd-grep.txt"));

    dir.write("ctx-cwd-find.txt", "find me");
    let result = executor
        .run(
            "find",
            &json!({ "pattern": "ctx-cwd-find.txt" }),
            &abort,
            None,
        )
        .await
        .expect("find");
    assert!(text_of(&result).contains("ctx-cwd-find.txt"));

    dir.write("ctx-cwd-ls.txt", "list me");
    let result = executor
        .run("ls", &json!({}), &abort, None)
        .await
        .expect("ls");
    assert!(text_of(&result).contains("ctx-cwd-ls.txt"));

    let result = executor
        .run("bash", &json!({ "command": "pwd" }), &abort, None)
        .await
        .expect("bash");
    assert!(
        text_of(&result).contains(&dir.path().display().to_string()),
        "{}",
        text_of(&result)
    );
}

// --- path-utils.test.ts -----------------------------------------------------

#[test]
fn expand_path_expands_tilde_to_the_home_directory() {
    let result = pi_coding_agent::tools::path_utils::expand_path("~");
    assert!(
        !result.display().to_string().contains('~'),
        "{}",
        result.display()
    );
    let result = pi_coding_agent::tools::path_utils::expand_path("~/Documents/file.txt");
    assert!(
        !result.display().to_string().contains("~/"),
        "{}",
        result.display()
    );
}

#[test]
fn expand_path_keeps_tilde_prefixed_filenames_literal() {
    assert_eq!(
        pi_coding_agent::tools::path_utils::expand_path("~draft.md")
            .display()
            .to_string(),
        "~draft.md"
    );
    assert_eq!(
        pi_coding_agent::tools::path_utils::expand_path("@~draft.md")
            .display()
            .to_string(),
        "~draft.md"
    );
}

#[test]
fn expand_path_normalizes_unicode_spaces() {
    assert_eq!(
        pi_coding_agent::tools::path_utils::expand_path("file\u{00A0}name.txt")
            .display()
            .to_string(),
        "file name.txt"
    );
}

#[test]
fn resolve_to_cwd_resolves_absolute_paths_as_is() {
    let absolute = std::env::temp_dir()
        .join("pi-rs-absolute")
        .join("path")
        .join("file.txt");
    let result = pi_coding_agent::tools::path_utils::resolve_to_cwd(
        &absolute.display().to_string(),
        std::path::Path::new("/some/cwd"),
    );
    assert_eq!(result, absolute);
}

#[test]
fn resolve_to_cwd_resolves_relative_paths_against_cwd() {
    let result = pi_coding_agent::tools::path_utils::resolve_to_cwd(
        "relative/file.txt",
        std::path::Path::new("/some/cwd"),
    );
    assert_eq!(
        result,
        std::path::Path::new("/some/cwd").join("relative/file.txt")
    );
}

#[test]
fn resolve_to_cwd_resolves_tilde_prefixed_filenames_against_cwd() {
    let cwd = std::env::temp_dir().join("pi-rs-path-utils-cwd");
    assert_eq!(
        pi_coding_agent::tools::path_utils::resolve_to_cwd("~draft.md", &cwd),
        cwd.join("~draft.md")
    );
    assert_eq!(
        pi_coding_agent::tools::path_utils::resolve_to_cwd("@~draft.md", &cwd),
        cwd.join("~draft.md")
    );
}

#[test]
fn resolve_read_path_resolves_existing_files_and_variants() {
    let dir = TempDir::new("path-read-variants");
    let file = dir.write("test-file.txt", "content");
    assert_eq!(
        pi_coding_agent::tools::path_utils::resolve_read_path("test-file.txt", dir.path()),
        file
    );
    // A missing path resolves to itself.
    assert_eq!(
        pi_coding_agent::tools::path_utils::resolve_read_path("missing.txt", dir.path()),
        dir.join("missing.txt")
    );
}

// --- coding/read-only families ----------------------------------------------

#[test]
fn coding_and_read_only_families_match_pi() {
    assert_eq!(
        ToolRegistry::coding().names(),
        vec!["read", "bash", "edit", "write"],
        "createCodingToolDefinitions"
    );
    assert_eq!(
        ToolRegistry::read_only().names(),
        vec!["read", "grep", "find", "ls"],
        "createReadOnlyToolDefinitions"
    );
    let unknown = pi_coding_agent::tools::create_tool_definition("nope");
    assert!(unknown.is_none());
}
