//! The write tool.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/write.ts`.
//!
//! Whole-file writes with parent directory creation, run inside the per-file
//! mutation queue. Abort is observed between the async steps exactly like
//! pi's `throwIfAborted` (`write.ts:71-73`), so the queue stays locked until
//! the in-flight filesystem operation has settled.

use std::fs;
use std::path::{Path, PathBuf};

use pi_agent::types::AbortSignal;
use pi_agent::types::AgentToolResult;

use crate::tools::mutation_queue::with_file_mutation_queue;
use crate::tools::path_utils::resolve_to_cwd;
use serde_json::Value;

use crate::tools::{ToolContext, ToolError, optional_str, text_result};

/// pi's `createWriteToolDefinition(...).execute` (`write.ts:57-89`).
pub async fn execute(
    arguments: &Value,
    context: &ToolContext,
    signal: &AbortSignal,
) -> Result<AgentToolResult, ToolError> {
    let path = optional_str(arguments, "path")
        .ok_or_else(|| ToolError("Expected required property 'path'".to_string()))?;
    let file_content = optional_str(arguments, "content")
        .ok_or_else(|| ToolError("Expected required property 'content'".to_string()))?;
    let absolute_path = resolve_to_cwd(&path, &context.cwd);
    // `dirname(absolutePath)` (`write.ts:65`).
    let dir: PathBuf = absolute_path
        .parent()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);

    let queue_path = absolute_path.clone();
    with_file_mutation_queue(&queue_path, move || async move {
        let throw_if_aborted = |signal: &AbortSignal| -> Result<(), ToolError> {
            if signal.aborted() {
                return Err(ToolError("Operation aborted".to_string()));
            }
            Ok(())
        };

        throw_if_aborted(signal)?;
        // Create parent directories if needed (`write.ts:77`).
        fs::create_dir_all(&dir).map_err(|error| ToolError(error.to_string()))?;
        throw_if_aborted(signal)?;

        // Write the file contents (`write.ts:81`).
        fs::write(&absolute_path, file_content.as_bytes())
            .map_err(|error| ToolError(error.to_string()))?;
        throw_if_aborted(signal)?;

        Ok(text_result(format!("Successfully wrote to {path}")))
    })
    .await
}

/// pi's write tool description (`write.ts:51-52`).
pub fn description() -> String {
    "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_string()
}

/// pi's `writeToolSystemPromptContribution` (`write.ts:15-18`).
pub const PROMPT_SNIPPET: &str = "Create or overwrite files";
/// pi's `writeToolSystemPromptContribution.guidelines`.
pub const PROMPT_GUIDELINES: [&str; 1] = ["Use write only for new files or complete rewrites."];
