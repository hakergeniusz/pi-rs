//! Built-in tool renderers, without the tools themselves.
//!
//! Ground truth: pi-mono
//! `packages/coding-agent/src/core/tools/renderers/index.ts` plus the
//! per-tool `renderers/*.ts`.
//!
//! The interactive renderers draw real TUI components (themes, previews,
//! timers); that is phase P6. What this module ports is each renderer's
//! *shape*: the call line and the result body it displays, rendered here as
//! markdown text a `pi_tui::components::markdown::Markdown` component takes
//! as input. The functions are pure and theme-free; the TUI phase can layer
//! styling on top without re-deriving what to show.

/// A tool's presentation functions.
#[derive(Clone, Copy)]
pub struct ToolRenderers {
    /// pi's `renderCall`: the line shown while the tool runs.
    pub render_call: fn(args: Option<&serde_json::Value>, cwd: &str) -> String,
    /// pi's `renderResult`: the body shown for a result.
    /// `expanded` is pi's `options.expanded`.
    pub render_result: fn(result: &pi_agent::types::AgentToolResult, expanded: bool) -> String,
}

/// Renderers for every built-in tool, keyed by tool name — pi's
/// `createAllToolRenderers()`. The skipped Windows-only `powershell` variant
/// renders as the shell renderer with `PS>` for symmetry.
pub fn create_all_tool_renderers() -> Vec<(&'static str, ToolRenderers)> {
    vec![
        ("read", super::read::READ_RENDERERS),
        ("bash", super::bash::create_shell_renderers("$")),
        ("powershell", super::bash::create_shell_renderers("PS>")),
        ("edit", super::edit::EDIT_RENDERERS),
        ("write", super::write::WRITE_RENDERERS),
        ("grep", super::grep::GREP_RENDERERS),
        ("find", super::find::FIND_RENDERERS),
        ("ls", super::ls::LS_RENDERERS),
    ]
}

/// The renderers for one tool name, or pi's `undefined`.
pub fn renderers_for(tool_name: &str) -> Option<ToolRenderers> {
    create_all_tool_renderers()
        .into_iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, renderers)| renderers)
}

/// pi's `withBuiltInRenderers` (`renderers/index.ts:60-71`): merge built-in
/// renderers into a tool that does not supply its own. This port's tool
/// definitions all carry renderers, so the merge is the lookup itself.
pub fn with_built_in_renderers(tool_name: &str) -> Option<ToolRenderers> {
    renderers_for(tool_name)
}
