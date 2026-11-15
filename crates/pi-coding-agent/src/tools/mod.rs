//! Built-in tools (`core/tools/`).
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/tools/*`, rebased on
//! pi-agent's [`pi_agent::types::ToolSpec`] / [`pi_agent::types::ToolExecutor`]
//! seam.
//!
//! Layout (one module per TS file):
//!
//! * [`truncate`] — the shared output limits (`truncate.ts`).
//! * [`output_accumulator`] — the bounded-memory streaming accumulator
//!   (`output-accumulator.ts`).
//! * [`path_utils`] — path resolution (`path-utils.ts` + `utils/paths.ts`).
//! * [`mutation_queue`] — per-file mutation serialization
//!   (`file-mutation-queue.ts`).
//! * [`edit_diff`] — the edit tool's matcher, line-ending preservation, and
//!   diffs (`edit-diff.ts`).
//! * [`render_utils`] — result text shaping (`render-utils.ts`).
//! * [`schema`] — argument preparation, coercion, and validation
//!   (`packages/ai` `validation.ts`, applied by the registry the way
//!   `tool-definition-wrapper.ts` wires it).
//! * [`read`] / [`bash`] / [`edit`] / [`write`] / [`ls`] / [`find`] /
//!   [`grep`] — one module per tool.
//! * [`renderers`] — presentation text for later TUI use (`renderers/*.ts`),
//!   rendered as markdown a `pi_tui` Markdown component takes.
//! * [`index`] — the registry, the JSON schemas, and the
//!   [`pi_agent::types::ToolExecutor`] implementation (`index.ts` +
//!   `tool-definition-wrapper.ts`).
//!
//! Windows-only `powershell.ts` is deliberately skipped and recorded as an
//! unsupported variant in [`index::UNSUPPORTED_TOOLS`].

/// The bash tool (`bash.ts`).
pub mod bash;
/// The file-edit tool (`edit.ts`).
pub mod edit;
/// The edit tool's matcher, line endings, and diffs (`edit-diff.ts`).
pub mod edit_diff;
/// The file-find tool (`find.ts`).
pub mod find;
/// The content-grep tool (`grep.ts`).
pub mod grep;
/// The tool registry: schemas, execution, and the [`pi_agent::types::ToolExecutor`] impl (`index.ts`).
pub mod index;
/// The directory-listing tool (`ls.ts`).
pub mod ls;
/// Per-file mutation serialization (`file-mutation-queue.ts`).
pub mod mutation_queue;
/// Bounded-memory accumulator for streaming tool output (`output-accumulator.ts`).
pub mod output_accumulator;
/// Path resolution for the file tools (`path-utils.ts` + `utils/paths.ts`).
pub mod path_utils;
/// The file-read tool (`read.ts`).
pub mod read;
/// Tool-result text shaping (`render-utils.ts`).
pub mod render_utils;
/// Presentation text for the TUI tool rows (`renderers/*.ts`).
pub mod renderers;
/// Tool-argument preparation, coercion, and validation (`validation.ts`).
pub mod schema;
/// The shared output limits (`truncate.ts`).
pub mod truncate;
/// The file-write tool (`write.ts`).
pub mod write;

pub use index::{
    BuiltinToolExecutor, TOOL_NAMES, ToolDefinition, ToolRegistry, UNSUPPORTED_TOOLS,
    all_tool_definitions, bash_definition, coding_tool_definitions, create_tool_definition,
    edit_definition, find_definition, grep_definition, ls_definition, read_definition,
    read_only_tool_definitions, write_definition,
};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pi_agent::types::AgentToolResult;
use pi_ai::types::{ContentBlock, TextContent};
use serde_json::Value;

/// A tool failure pi throws rather than returns as `isError`.
///
/// The loop turns these into error tool results, so the distinction from
/// [`AgentToolResult::is_error`] is preserved: bash reports a non-zero exit as
/// an `is_error` *result* with output attached, while a timeout or a missing
/// working directory is a thrown error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

impl std::fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Everything a tool needs from its host: the parts of pi's
/// `ExtensionContext` the builtins actually read (`ctx.cwd`, the shell
/// environment, session metadata) plus the `*ToolOptions` the session passes.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// The working directory tools resolve relative paths against.
    pub cwd: PathBuf,
    /// The environment a shell tool runs with. Defaults to the process env
    /// with pi's bin directory prepended to `PATH` (pi's `getShellEnv`).
    pub env: BTreeMap<String, String>,
    /// `PI_*` session variables, exposed only when `expose_session_environment`.
    pub session_env: BTreeMap<String, String>,
    /// pi's `exposeSessionEnvironment` (`bash.ts:250`), default `true`.
    pub expose_session_environment: bool,
    /// pi's `commandPrefix` (`bash.ts:246`), prepended to every shell command.
    pub command_prefix: Option<String>,
    /// pi's `shellPath` (`bash.ts:248`); when unset `/bin/bash`, then `bash`
    /// on `PATH`, then `sh`.
    pub shell_path: Option<String>,
}

impl ToolContext {
    /// A context rooted at `cwd`, with pi's `getShellEnv` as the environment.
    pub fn new(cwd: impl Into<PathBuf>) -> ToolContext {
        ToolContext {
            cwd: cwd.into(),
            env: shell_env(&parent_environment(), &bin_dir())
                .into_iter()
                .collect(),
            session_env: BTreeMap::new(),
            expose_session_environment: true,
            command_prefix: None,
            shell_path: None,
        }
    }

    /// pi's `resolveSpawnContext` environment half (`bash.ts:214-240`): the
    /// base env with the `PI_*` session variables when exposure is on. pi
    /// deletes the session variables from the inherited env first so a stale
    /// value cannot leak through.
    pub fn shell_env(&self) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        for key in [
            "PI_SESSION_ID",
            "PI_SESSION_FILE",
            "PI_PROVIDER",
            "PI_MODEL",
            "PI_REASONING_LEVEL",
        ] {
            env.remove(key);
        }
        if self.expose_session_environment {
            for (key, value) in &self.session_env {
                env.insert(key.clone(), value.clone());
            }
        }
        env
    }
}

/// pi's `{ type: "text", text }` result with pi's other fields empty.
pub fn text_result(text: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        ..AgentToolResult::default()
    }
}

/// A text content block.
pub(crate) fn text_block(text: String) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text,
        text_signature: None,
    })
}

/// The required string argument, or pi's missing-property failure.
pub(crate) fn required_str(arguments: &Value, key: &str) -> Result<String, ToolError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError(format!("Expected required property '{key}'")))
}

/// An optional string argument.
pub(crate) fn optional_str(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// An optional numeric argument. Validation has already coerced it.
pub(crate) fn optional_f64(arguments: &Value, key: &str) -> Option<f64> {
    arguments.get(key).and_then(Value::as_f64)
}

/// An optional boolean argument.
pub(crate) fn optional_bool(arguments: &Value, key: &str) -> bool {
    arguments.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Node's `error.message` shape for a filesystem error, which is what pi
/// surfaces verbatim when the read tool lets a raw fs error escape
/// (`ENOENT: no such file or directory, access '/tmp/x'`).
pub(crate) fn io_error_node_message(error: &std::io::Error, path: &Path, syscall: &str) -> String {
    let (code, message) = match error.raw_os_error() {
        Some(1) => ("EPERM", "operation not permitted"),
        Some(2) => ("ENOENT", "no such file or directory"),
        Some(13) => ("EACCES", "permission denied"),
        Some(20) => ("ENOTDIR", "not a directory"),
        Some(21) => ("EISDIR", "illegal operation on a directory"),
        Some(40) => ("ELOOP", "too many symbolic links encountered"),
        _ => ("UNKNOWN", "unknown error"),
    };
    format!("{code}: {message}, {syscall} '{}'", path.display())
}

/// Node's `error.code` string for a filesystem error, the shape pi embeds in
/// `Could not edit file: <path>. Error code: ENOENT.` (`edit.ts:174-176`).
pub(crate) fn io_error_code(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(1) => "Error code: EPERM".to_string(),
        Some(2) => "Error code: ENOENT".to_string(),
        Some(13) => "Error code: EACCES".to_string(),
        Some(17) => "Error code: EEXIST".to_string(),
        Some(20) => "Error code: ENOTDIR".to_string(),
        Some(21) => "Error code: EISDIR".to_string(),
        Some(28) => "Error code: ENOSPC".to_string(),
        Some(36) => "Error code: ENAMETOOLONG".to_string(),
        Some(40) => "Error code: ELOOP".to_string(),
        Some(code) => format!("Error code: E{code}"),
        None => format!("Error: {error}"),
    }
}

/// The process environment as pairs, which makes [`shell_env`] a pure function
/// a test can drive without mutating the test process's own environment.
pub fn parent_environment() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// pi's `getBinDir` (`config.ts:597-599`): where pi installs the shims a
/// session's own `pi` resolves to. Delegated to
/// [`crate::config::get_bin_dir`] — the XDG cache root by default, the agent
/// dir when `PI_CODING_AGENT_DIR` is set.
pub fn bin_dir() -> PathBuf {
    crate::config::get_bin_dir()
}

/// pi's `CONFIG_DIR_NAME` (`config.ts:606`): the project config directory,
/// `.pi` unless the package config renames it. Aliased from
/// [`crate::config::PROJECT_CONFIG_DIR_NAME`] so there is one source of
/// truth; the user-level XDG leaf is `crate::config::CONFIG_DIR_NAME`.
pub const CONFIG_DIR_NAME: &str = crate::config::PROJECT_CONFIG_DIR_NAME;

/// pi's `getAgentDir` (`config.ts:630-635`), delegated to
/// [`crate::config::get_agent_dir`]: `$PI_CODING_AGENT_DIR` when set, else
/// the XDG config root (upstream: `~/.pi/agent`).
pub fn agent_dir() -> PathBuf {
    crate::config::get_agent_dir()
}

/// pi's `getShellEnv` (`utils/shell.ts:138-151`): the process environment with
/// pi's own bin directory prepended to `PATH`, and nothing else changed.
///
/// `parent` is the environment to start from, so the prepend is testable
/// without `std::env::set_var` (a cross-test race even where it is allowed).
pub fn shell_env(parent: &[(String, String)], bin_dir: &Path) -> Vec<(String, String)> {
    let path_key = parent
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map_or_else(|| "PATH".to_string(), |(key, _)| key.clone());
    let current = parent
        .iter()
        .find(|(key, _)| *key == path_key)
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let separator = if cfg!(windows) { ";" } else { ":" };
    let entries: Vec<&str> = current
        .split(separator)
        .filter(|entry| !entry.is_empty())
        .collect();
    let bin = bin_dir.to_string_lossy().to_string();
    // pi does not prepend an entry that is already on the path.
    let updated = if entries.iter().any(|entry| *entry == bin) {
        current
    } else {
        [bin.as_str(), current.as_str()]
            .into_iter()
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>()
            .join(separator)
    };
    let mut env: Vec<(String, String)> = Vec::with_capacity(parent.len() + 1);
    let mut replaced = false;
    for (key, value) in parent {
        if *key == path_key {
            env.push((path_key.clone(), updated.clone()));
            replaced = true;
        } else {
            env.push((key.clone(), value.clone()));
        }
    }
    if !replaced {
        env.push((path_key, updated));
    }
    env
}
