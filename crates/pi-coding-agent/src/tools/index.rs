//! The builtin tool registry.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/index.ts`
//! (the `create*Tools` families) and `tool-definition-wrapper.ts` (the
//! definition-to-tool seam, which here becomes the
//! [`pi_agent::types::ToolExecutor`] implementation).
//!
//! What this module owns:
//!
//! * [`ToolDefinition`] — the description, prompt contribution, parameters
//!   schema, optional output schema, `prepareArguments`, execution mode, and
//!   renderers. The schemas are transcribed from pi's `TypeBox` definitions,
//!   key for key, including the descriptions.
//! * [`ToolRegistry`] — ordered lookup by name and the `create*Tools`
//!   families (`index.ts:164-211`).
//! * [`BuiltinToolExecutor`] — pi-agent's [`pi_agent::types::ToolExecutor`]
//!   over the registry, running `prepareArguments` and schema validation
//!   before the tool body, which is where pi's agent loop puts them.
//!
//! `powershell` is registered in [`UNSUPPORTED_TOOLS`] rather than silently
//! dropped: pi ships it for Windows shells, this port targets the same
//! platforms as the bash tool. The `codemode` extension tool is reachable by
//! name through [`create_tool_definition`] with inert defaults
//! (`extensions/codemode/index.ts:32-46`); it is deliberately absent from the
//! default tool set, matching upstream's `defaultActive: false`.

use std::path::PathBuf;
use std::sync::Arc;

use pi_agent::types::{
    AgentToolCall, AgentToolResult, BoxFuture, ConstrainedSampling, StrictMode, ToolExecutor,
    ToolSpec,
};
use serde_json::{Value, json};

use crate::tools::bash;
use crate::tools::edit;
use crate::tools::find;
use crate::tools::grep;
use crate::tools::ls;
use crate::tools::read;
use crate::tools::renderers::ToolRenderers;
use crate::tools::schema::{PrepareArguments, validate_tool_arguments};
use crate::tools::write;
use crate::tools::{ToolContext, ToolError, text_block};

/// pi's `ToolName` (`index.ts:95`).
pub const TOOL_NAMES: [&str; 8] = [
    "read",
    "bash",
    "powershell",
    "edit",
    "write",
    "grep",
    "find",
    "ls",
];

/// Tools pi ships that this port does not implement, with the reason.
pub const UNSUPPORTED_TOOLS: [(&str, &str); 1] = [(
    "powershell",
    "Windows-only shell variant (powershell.ts); this port targets the same platforms as the bash tool",
)];

/// pi's `constrainedSampling` on the four strict tools
/// (`{ type: "json_schema", strict: "prefer" }`).
const STRICT_PREFER: ConstrainedSampling = ConstrainedSampling::JsonSchema {
    strict: StrictMode::Prefer,
};

/// A tool as the model sees it, plus what the host needs to run it — pi's
/// `ToolDefinition` (`extensions/types.ts`) reduced to what the builtins
/// carry.
#[derive(Clone)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub label: &'static str,
    pub description: String,
    /// JSON Schema for the arguments, transcribed from pi's `TypeBox` schema.
    pub parameters: Value,
    /// JSON Schema for `structuredContent`, where pi declares one.
    pub output_schema: Option<Value>,
    pub prompt_snippet: Option<&'static str>,
    pub prompt_guidelines: Vec<String>,
    /// pi's `constrainedSampling`.
    pub constrained_sampling: Option<ConstrainedSampling>,
    /// pi's `prepareArguments`, run before validation.
    pub prepare_arguments: Option<PrepareArguments>,
    /// pi's `executionMode` override.
    pub execution_mode: Option<pi_agent::types::ToolExecutionMode>,
    /// pi's `renderShell`.
    pub render_shell: Option<&'static str>,
    /// pi's `renderCall` / `renderResult`, as markdown shapers.
    pub renderers: ToolRenderers,
}

/// pi's read parameters (`read.ts:14-18`).
pub const READ_SCHEMA: &str = r#"{
  "type": "object",
  "required": [
    "path"
  ],
  "properties": {
    "path": {
      "type": "string",
      "description": "Path to the file to read (relative or absolute)"
    },
    "offset": {
      "type": "number",
      "description": "Line number to start reading from (1-indexed)"
    },
    "limit": {
      "type": "number",
      "description": "Maximum number of lines to read"
    }
  }
}"#;

/// pi's bash parameters (`bash.ts:39-42`).
pub const BASH_SCHEMA: &str = r#"{
  "type": "object",
  "required": [
    "command"
  ],
  "properties": {
    "command": {
      "type": "string",
      "description": "Shell command to execute"
    },
    "timeout": {
      "type": "number",
      "description": "Timeout in seconds (optional, no default timeout)"
    }
  }
}"#;

/// pi's bash `outputSchema` (`bash.ts:55-61`).
pub const BASH_OUTPUT_SCHEMA: &str = r#"{
  "type": "object",
  "required": [
    "output",
    "truncated",
    "exit_code",
    "wall_time_seconds"
  ],
  "properties": {
    "output": {
      "type": "string",
      "description": "Combined stdout and stderr, possibly truncated"
    },
    "truncated": {
      "type": "boolean"
    },
    "full_output_path": {
      "type": "string",
      "description": "Full output, when truncated"
    },
    "exit_code": {
      "type": "number"
    },
    "wall_time_seconds": {
      "type": "number"
    }
  }
}"#;

/// pi's read `outputSchema` (`read.ts:33-36`): the text for text files, and
/// an image block for images codemode's `image()` accepts.
pub const READ_OUTPUT_SCHEMA: &str = r#"{
  "anyOf": [
    {
      "type": "string"
    },
    {
      "type": "object",
      "required": [
        "type",
        "data",
        "mimeType",
        "note"
      ],
      "properties": {
        "type": {
          "const": "image"
        },
        "data": {
          "type": "string"
        },
        "mimeType": {
          "type": "string"
        },
        "note": {
          "type": "string"
        }
      }
    }
  ]
}"#;

/// pi's write parameters (`write.ts:10-13`).
pub const WRITE_SCHEMA: &str = r#"{
  "type": "object",
  "required": [
    "path",
    "content"
  ],
  "properties": {
    "path": {
      "type": "string",
      "description": "Path to the file to write (relative or absolute)"
    },
    "content": {
      "type": "string",
      "description": "Content to write to the file"
    }
  }
}"#;

/// pi's edit parameters (`edit.ts:18-38`).
pub const EDIT_SCHEMA: &str = r#"{
  "type": "object",
  "required": [
    "path",
    "edits"
  ],
  "properties": {
    "path": {
      "type": "string",
      "description": "Path to the file to edit (relative or absolute)"
    },
    "edits": {
      "type": "array",
      "items": {
        "type": "object",
        "required": [
          "oldText",
          "newText"
        ],
        "properties": {
          "oldText": {
            "type": "string",
            "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
          },
          "newText": {
            "type": "string",
            "description": "Replacement text for this targeted edit."
          }
        }
      },
      "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."
    }
  }
}"#;

/// pi's grep parameters (`grep.ts:20-37`).
pub const GREP_SCHEMA: &str = r#"{
  "type": "object",
  "required": [
    "pattern"
  ],
  "properties": {
    "pattern": {
      "type": "string",
      "description": "Search pattern (regex or literal string)"
    },
    "path": {
      "type": "string",
      "description": "Directory or file to search (default: current directory)"
    },
    "glob": {
      "type": "string",
      "description": "Filter files by glob pattern, relative to path, e.g. '*.ts', 'src/**/*.ts' or '**/*.spec.ts'. A pattern without '/' matches file names at any depth"
    },
    "ignoreCase": {
      "type": "boolean",
      "description": "Case-insensitive search (default: false)"
    },
    "literal": {
      "type": "boolean",
      "description": "Treat pattern as literal string instead of regex (default: false)"
    },
    "context": {
      "type": "number",
      "description": "Number of lines to show before and after each match (default: 0)"
    },
    "limit": {
      "type": "number",
      "description": "Maximum number of matches to return (default: 100)"
    }
  }
}"#;

/// pi's find parameters (`find.ts:26-32`).
pub const FIND_SCHEMA: &str = r#"{
  "type": "object",
  "required": [
    "pattern"
  ],
  "properties": {
    "pattern": {
      "type": "string",
      "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
    },
    "path": {
      "type": "string",
      "description": "Directory to search in (default: current directory)"
    },
    "limit": {
      "type": "number",
      "description": "Maximum number of results (default: 1000)"
    }
  }
}"#;

/// pi's ls parameters (`ls.ts:10-13`); no required fields.
pub const LS_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "description": "Directory to list (default: current directory)"
    },
    "limit": {
      "type": "number",
      "description": "Maximum number of entries to return (default: 500)"
    }
  }
}"#;

fn schema(source: &str) -> Value {
    serde_json::from_str(source).expect("builtin schema parses")
}

fn guidelines(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// pi's `createReadToolDefinition` (`read.ts:231-361`).
pub fn read_definition() -> ToolDefinition {
    ToolDefinition {
        name: "read",
        label: "read",
        description: read::description(),
        parameters: schema(READ_SCHEMA),
        output_schema: Some(schema(READ_OUTPUT_SCHEMA)),
        prompt_snippet: Some(read::PROMPT_SNIPPET),
        prompt_guidelines: guidelines(&read::PROMPT_GUIDELINES),
        constrained_sampling: Some(STRICT_PREFER),
        prepare_arguments: None,
        execution_mode: None,
        render_shell: None,
        renderers: crate::tools::renderers::read::READ_RENDERERS,
    }
}

/// pi's `createBashToolDefinition` (`bash.ts:461-466`).
pub fn bash_definition() -> ToolDefinition {
    ToolDefinition {
        name: "bash",
        label: "bash",
        description: bash::description(),
        parameters: schema(BASH_SCHEMA),
        output_schema: Some(schema(BASH_OUTPUT_SCHEMA)),
        prompt_snippet: Some(bash::PROMPT_SNIPPET),
        prompt_guidelines: guidelines(&bash::PROMPT_GUIDELINES),
        constrained_sampling: Some(STRICT_PREFER),
        prepare_arguments: None,
        execution_mode: None,
        render_shell: None,
        renderers: crate::tools::renderers::bash::create_shell_renderers("$"),
    }
}

/// pi's `createEditToolDefinition` (`edit.ts:138-217`).
pub fn edit_definition() -> ToolDefinition {
    ToolDefinition {
        name: "edit",
        label: "edit",
        description: edit::description(),
        parameters: schema(EDIT_SCHEMA),
        output_schema: None,
        prompt_snippet: Some(edit::PROMPT_SNIPPET),
        prompt_guidelines: guidelines(&edit::PROMPT_GUIDELINES),
        constrained_sampling: Some(STRICT_PREFER),
        prepare_arguments: Some(edit::PREPARE_ARGUMENTS),
        execution_mode: None,
        render_shell: Some(edit::RENDER_SHELL),
        renderers: crate::tools::renderers::edit::EDIT_RENDERERS,
    }
}

/// pi's `createWriteToolDefinition` (`write.ts:43-92`).
pub fn write_definition() -> ToolDefinition {
    ToolDefinition {
        name: "write",
        label: "write",
        description: write::description(),
        parameters: schema(WRITE_SCHEMA),
        output_schema: None,
        prompt_snippet: Some(write::PROMPT_SNIPPET),
        prompt_guidelines: guidelines(&write::PROMPT_GUIDELINES),
        constrained_sampling: Some(STRICT_PREFER),
        prepare_arguments: None,
        execution_mode: None,
        render_shell: None,
        renderers: crate::tools::renderers::write::WRITE_RENDERERS,
    }
}

/// pi's `createGrepToolDefinition` (`grep.ts:82-333`).
pub fn grep_definition() -> ToolDefinition {
    ToolDefinition {
        name: "grep",
        label: "grep",
        description: grep::description(),
        parameters: schema(GREP_SCHEMA),
        output_schema: None,
        prompt_snippet: Some(grep::PROMPT_SNIPPET),
        prompt_guidelines: Vec::new(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        render_shell: None,
        renderers: crate::tools::renderers::grep::GREP_RENDERERS,
    }
}

/// pi's `createFindToolDefinition` (`find.ts:70-357`).
pub fn find_definition() -> ToolDefinition {
    ToolDefinition {
        name: "find",
        label: "find",
        description: find::description(),
        parameters: schema(FIND_SCHEMA),
        output_schema: None,
        prompt_snippet: Some(find::PROMPT_SNIPPET),
        prompt_guidelines: Vec::new(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        render_shell: None,
        renderers: crate::tools::renderers::find::FIND_RENDERERS,
    }
}

/// pi's `createLsToolDefinition` (`ls.ts:53-171`).
pub fn ls_definition() -> ToolDefinition {
    ToolDefinition {
        name: "ls",
        label: "ls",
        description: ls::description(),
        parameters: schema(LS_SCHEMA),
        output_schema: None,
        prompt_snippet: Some(ls::PROMPT_SNIPPET),
        prompt_guidelines: Vec::new(),
        constrained_sampling: None,
        prepare_arguments: None,
        execution_mode: None,
        render_shell: None,
        renderers: crate::tools::renderers::ls::LS_RENDERERS,
    }
}

/// pi's `createToolDefinition`, by name (`index.ts:118-139`), extended with the
/// `codemode` extension tool (`extensions/codemode/index.ts:32-46`). Codemode
/// is looked up by name here so the coordinator can register its declaration
/// with inert defaults; unlike the core tools it is not in the default tool
/// set (upstream registers it `defaultActive: false`).
pub fn create_tool_definition(name: &str) -> Option<ToolDefinition> {
    match name {
        "read" => Some(read_definition()),
        "bash" => Some(bash_definition()),
        "edit" => Some(edit_definition()),
        "write" => Some(write_definition()),
        "grep" => Some(grep_definition()),
        "find" => Some(find_definition()),
        "ls" => Some(ls_definition()),
        "codemode" => Some(crate::codemode_host::declarative_tool_definition()),
        _ => None,
    }
}

/// pi's `createAllToolDefinitions` minus the unsupported powershell variant
/// (`index.ts:182-193`).
pub fn all_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        read_definition(),
        bash_definition(),
        edit_definition(),
        write_definition(),
        grep_definition(),
        find_definition(),
        ls_definition(),
    ]
}

/// pi's `createCodingToolDefinitions` (read, bash, edit, write)
/// (`index.ts:164-171`).
pub fn coding_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        read_definition(),
        bash_definition(),
        edit_definition(),
        write_definition(),
    ]
}

/// pi's `createReadOnlyToolDefinitions` (read, grep, find, ls)
/// (`index.ts:173-180`).
pub fn read_only_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        read_definition(),
        grep_definition(),
        find_definition(),
        ls_definition(),
    ]
}

/// An ordered set of tools with lookup — pi's tool arrays plus the name
/// lookup the session registry does.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    definitions: Vec<ToolDefinition>,
}

impl ToolRegistry {
    pub fn new(definitions: Vec<ToolDefinition>) -> ToolRegistry {
        ToolRegistry { definitions }
    }

    /// pi's `createAllTools` minus the unsupported powershell variant.
    pub fn all() -> ToolRegistry {
        ToolRegistry::new(all_tool_definitions())
    }

    /// pi's `createCodingTools` (`index.ts:195-202`).
    pub fn coding() -> ToolRegistry {
        ToolRegistry::new(coding_tool_definitions())
    }

    /// pi's `createReadOnlyTools` (`index.ts:204-211`).
    pub fn read_only() -> ToolRegistry {
        ToolRegistry::new(read_only_tool_definitions())
    }

    /// The registered tools, in declaration order.
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    /// pi's `tools.find((t) => t.name === name)`.
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.definitions.iter().find(|tool| tool.name == name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// The tool names, in declaration order.
    pub fn names(&self) -> Vec<&str> {
        self.definitions.iter().map(|tool| tool.name).collect()
    }

    /// The pi-agent `ToolSpec` list this registry presents to the loop —
    /// pi's `wrapToolDefinition` output (`tool-definition-wrapper.ts:27-49`).
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.definitions
            .iter()
            .map(|tool| ToolSpec {
                name: tool.name.to_string(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
                execution_mode: tool.execution_mode,
                constrained_sampling: tool.constrained_sampling.clone(),
            })
            .collect()
    }

    /// The executor for this registry, bound to one working directory.
    pub fn executor(&self, cwd: impl Into<PathBuf>) -> Arc<BuiltinToolExecutor> {
        Arc::new(BuiltinToolExecutor::new(
            self.clone(),
            ToolContext::new(cwd),
        ))
    }
}

/// pi-agent's [`ToolExecutor`] over a [`ToolRegistry`].
///
/// `prepareArguments` and schema validation run here, in pi's order, because
/// the loop calls `execute` with whatever the model produced.
pub struct BuiltinToolExecutor {
    registry: ToolRegistry,
    context: ToolContext,
}

impl BuiltinToolExecutor {
    pub fn new(registry: ToolRegistry, context: ToolContext) -> BuiltinToolExecutor {
        BuiltinToolExecutor { registry, context }
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn context(&self) -> &ToolContext {
        &self.context
    }

    /// Execute one already-validated call. The [`ToolExecutor`] impl is the
    /// public seam; this is what tests and hosts drive directly.
    pub async fn run(
        &self,
        name: &str,
        arguments: &Value,
        signal: &pi_agent::types::AbortSignal,
        updates: Option<&tokio::sync::mpsc::UnboundedSender<AgentToolResult>>,
    ) -> Result<AgentToolResult, String> {
        let definition = self
            .registry
            .get(name)
            .ok_or_else(|| format!("Tool {name} not found"))?;
        let arguments = validate_tool_arguments(
            name,
            &definition.parameters,
            arguments,
            definition.prepare_arguments,
        )?;

        let result = match name {
            "bash" => bash::execute(&arguments, &self.context, signal, updates).await,
            "edit" => edit::execute(&arguments, &self.context, signal).await,
            "write" => write::execute(&arguments, &self.context, signal).await,
            "read" => read::execute(&arguments, &self.context, signal).await,
            "grep" => grep::execute(&arguments, &self.context, signal).await,
            "find" => find::execute(&arguments, &self.context, signal).await,
            "ls" => ls::execute(&arguments, &self.context, signal).await,
            other => Err(ToolError(format!("Tool {other} not found"))),
        };
        result.map_err(|error: ToolError| error.0)
    }
}

impl ToolExecutor for BuiltinToolExecutor {
    fn tools(&self) -> Vec<ToolSpec> {
        self.registry.tool_specs()
    }

    fn execution_mode(&self, name: &str) -> Option<pi_agent::types::ToolExecutionMode> {
        self.registry.get(name).and_then(|tool| tool.execution_mode)
    }

    fn execute(
        &self,
        call: AgentToolCall,
        updates: tokio::sync::mpsc::UnboundedSender<AgentToolResult>,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        let name = call.name.clone();
        let arguments = Value::Object(call.arguments.clone());
        Box::pin(async move {
            // pi's `execute` receives an `AgentToolCall`; the id is unused by
            // the builtins, which report through the result.
            let _ = call.id;
            self.run(
                &name,
                &arguments,
                &pi_agent::types::AbortSignal::new(),
                Some(&updates),
            )
            .await
        })
    }
}

/// A result with exactly one text block, for tests and error results.
#[allow(dead_code)]
pub(crate) fn text_result_with(text: String) -> AgentToolResult {
    AgentToolResult {
        content: vec![text_block(text)],
        ..AgentToolResult::default()
    }
}

/// The JSON schemas as the model sees them, for schema-compat tests.
pub fn schema_map() -> Vec<(&'static str, Value)> {
    all_tool_definitions()
        .into_iter()
        .map(|definition| (definition.name, definition.parameters))
        .collect()
}

/// pi's `allToolNames` as a JSON array, for prompt assembly.
#[allow(dead_code)]
pub fn all_tool_names_json() -> Value {
    json!(TOOL_NAMES)
}
