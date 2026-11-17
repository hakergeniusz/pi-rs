//! The edit tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/edit.ts`,
//! `edit-diff.ts`, and `file-mutation-queue.ts`.
//!
//! Behaviors ported:
//!
//! * every `oldText` must match exactly once; zero matches, several matches,
//!   overlapping edits, an empty `oldText`, and a no-op edit are distinct
//!   errors with pi's messages (`edit-diff.ts:296-332`);
//! * edits are matched against the original file, not incrementally
//!   (`edit-diff.ts:343-406`);
//! * a BOM is stripped before matching and restored on write (`edit.ts:186`),
//!   and untouched lines keep their line endings via
//!   `apply_replacements_to_original` (`edit.ts:199-200`);
//! * the diff is computed before writing, so diff computation cannot fail
//!   after the write (`edit.ts:193-194`);
//! * the result is `Successfully replaced N block(s) in <path>.` with the
//!   display diff, unified patch, and first changed line in `details`;
//! * the whole operation runs inside the file mutation queue
//!   (`edit.ts:158`);
//! * `prepareArguments` tolerates the shapes models actually send
//!   (`edit.ts:98-129`).

use std::fs;

use pi_agent::types::AbortSignal;
use pi_agent::types::AgentToolResult;
use serde_json::{Map, Value, json};

use crate::tools::edit_diff::{
    apply_edits_to_normalized_content, apply_replacements_to_original, generate_edit_diff,
    normalize_to_lf, split_bom,
};
use crate::tools::mutation_queue::with_file_mutation_queue;
use crate::tools::path_utils::resolve_to_cwd;
use crate::tools::schema::PrepareArguments;
use crate::tools::{ToolContext, ToolError, io_error_code, optional_str, text_block};

/// pi's `validateEditInput` (`edit.ts:131-136`).
fn validate_edit_input(
    arguments: &Value,
) -> Result<(String, Vec<crate::tools::edit_diff::Edit>), ToolError> {
    let edits = arguments
        .get("edits")
        .and_then(Value::as_array)
        .filter(|edits| !edits.is_empty())
        .ok_or_else(|| {
            ToolError(
                "Edit tool input is invalid. edits must contain at least one replacement."
                    .to_string(),
            )
        })?;
    let path = optional_str(arguments, "path")
        .ok_or_else(|| ToolError("Expected required property 'path'".to_string()))?;
    let mut parsed = Vec::with_capacity(edits.len());
    for edit in edits {
        let old_text = edit
            .get("oldText")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError("Expected required property 'oldText'".to_string()))?;
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError("Expected required property 'newText'".to_string()))?;
        parsed.push(crate::tools::edit_diff::Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        });
    }
    Ok((path, parsed))
}

fn is_single_edit_input(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("oldText").is_some_and(Value::is_string)
        && object.get("newText").is_some_and(Value::is_string)
}

/// pi's `prepareEditArguments` (`edit.ts:98-129`).
///
/// Three model quirks, in pi's order: `edits` as a JSON string
/// (Opus 4.6, GLM-5.1), `edits` as a single edit object instead of a
/// one-element array, and the legacy top-level `oldText`/`newText` pair,
/// which is appended to whatever `edits` holds.
pub fn prepare_edit_arguments(input: Value) -> Value {
    let Some(object) = input.as_object() else {
        return input;
    };
    let mut args = object.clone();

    let edits_is_string = args.get("edits").is_some_and(Value::is_string);
    let edits_is_single = args.get("edits").is_some_and(is_single_edit_input);
    if edits_is_string {
        if let Some(text) = args.get("edits").and_then(Value::as_str)
            && let Ok(parsed) = serde_json::from_str::<Value>(text)
        {
            if parsed.is_array() {
                args.insert("edits".to_string(), parsed);
            } else if is_single_edit_input(&parsed) {
                args.insert("edits".to_string(), Value::Array(vec![parsed]));
            }
        }
    } else if edits_is_single {
        let single = args.get("edits").cloned().expect("checked above");
        args.insert("edits".to_string(), Value::Array(vec![single]));
    }

    let legacy_old = args
        .get("oldText")
        .and_then(Value::as_str)
        .map(str::to_string);
    let legacy_new = args
        .get("newText")
        .and_then(Value::as_str)
        .map(str::to_string);
    let (Some(old_text), Some(new_text)) = (legacy_old, legacy_new) else {
        return Value::Object(args);
    };

    let mut edits = match args.get("edits") {
        Some(Value::Array(existing)) => existing.clone(),
        _ => Vec::new(),
    };
    edits.push(json!({ "oldText": old_text, "newText": new_text }));
    args.insert("edits".to_string(), Value::Array(edits));
    args.remove("oldText");
    args.remove("newText");
    Value::Object(args)
}

/// pi's `createEditToolDefinition(...).execute` (`edit.ts:154-214`).
pub async fn execute(
    arguments: &Value,
    context: &ToolContext,
    signal: &AbortSignal,
) -> Result<AgentToolResult, ToolError> {
    let (path, edits) = validate_edit_input(arguments)?;
    let absolute_path = resolve_to_cwd(&path, &context.cwd);

    // Do not reject from an abort listener at the queue boundary: that would
    // release the mutation queue while an in-flight filesystem operation may
    // still finish (`edit.ts:159-166`).
    with_file_mutation_queue(&absolute_path, || async {
        let throw_if_aborted = |signal: &AbortSignal| -> Result<(), ToolError> {
            if signal.aborted() {
                return Err(ToolError("Operation aborted".to_string()));
            }
            Ok(())
        };

        throw_if_aborted(signal)?;

        // Check if file exists (`edit.ts:170-177`).
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&absolute_path)
            .map_err(|error| {
                ToolError(format!(
                    "Could not edit file: {path}. {}.",
                    match error.raw_os_error() {
                        Some(_) => io_error_code(&error),
                        // pi renders a non-OS error as `Error: <message>`
                        // (`edit.ts:174-175`).
                        None => format!("Error: {error}"),
                    }
                ))
            })?;
        throw_if_aborted(signal)?;

        // Read the file (`edit.ts:181-183`).
        let raw_content = fs::read_to_string(&absolute_path).map_err(|error| {
            ToolError(format!(
                "Could not edit file: {path}. {}.",
                io_error_code(&error)
            ))
        })?;
        throw_if_aborted(signal)?;

        // Strip BOM before matching. The model will not include an invisible
        // BOM in oldText (`edit.ts:185-192`).
        let (bom, file_content) = split_bom(&raw_content);
        let normalized_content = normalize_to_lf(&file_content);
        let applied = apply_edits_to_normalized_content(&normalized_content, &edits, &path)
            .map_err(ToolError)?;

        // Diff before writing, so diff computation cannot fail after the
        // write (`edit.ts:193-194`).
        let edit_diff = generate_edit_diff(
            &path,
            &applied.base_content,
            &applied.new_content,
            &applied.replacements,
            4,
        );
        throw_if_aborted(signal)?;

        // Splice into the original text, so untouched lines keep their line
        // endings (`edit.ts:197-201`).
        let spliced = if file_content == normalized_content {
            applied.new_content.clone()
        } else {
            apply_replacements_to_original(&file_content, &applied.replacements)
        };
        let final_content = format!("{bom}{spliced}");
        fs::write(&absolute_path, final_content.as_bytes()).map_err(|error| {
            ToolError(format!(
                "Could not edit file: {path}. {}.",
                io_error_code(&error)
            ))
        })?;
        throw_if_aborted(signal)?;

        Ok(AgentToolResult {
            content: vec![text_block(format!(
                "Successfully replaced {} block(s) in {path}.",
                edits.len()
            ))],
            details: edit_details_value(
                &edit_diff.diff,
                &edit_diff.patch,
                edit_diff.first_changed_line,
            ),
            ..AgentToolResult::default()
        })
    })
    .await
}

/// pi's `EditToolDetails` JSON shape: `firstChangedLine` is omitted when
/// absent, the other two keys always present.
pub(crate) fn edit_details_value(
    diff: &str,
    patch: &str,
    first_changed_line: Option<usize>,
) -> Value {
    let mut details = Map::new();
    details.insert("diff".to_string(), json!(diff));
    details.insert("patch".to_string(), json!(patch));
    if let Some(line) = first_changed_line {
        details.insert("firstChangedLine".to_string(), json!(line));
    }
    Value::Object(details)
}

/// pi's edit tool description (`edit.ts:146-147`).
pub fn description() -> String {
    "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".to_string()
}

/// pi's `editToolSystemPromptContribution` (`edit.ts:40-46`).
pub const PROMPT_SNIPPET: &str = "Make precise file edits with exact text replacement, including multiple disjoint edits in one call";
/// pi's `editToolSystemPromptContribution.guidelines`.
pub const PROMPT_GUIDELINES: [&str; 2] = [
    "Use edit for targeted changes. Batch changes to one file in a single call.",
    "Each edits[].oldText must exactly and uniquely match the original file. Keep matches short and non-overlapping; merge nearby changes without including large unchanged regions.",
];

/// pi's `renderShell` value for the edit tool (`edit.ts:152`).
pub const RENDER_SHELL: &str = "self";

/// The hook pi registers as `prepareArguments` (`edit.ts:153`).
pub const PREPARE_ARGUMENTS: PrepareArguments = prepare_edit_arguments;
