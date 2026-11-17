//! The tool HTML renderer seam for custom tools in the HTML export.
//!
//! Ground truth: `packages/coding-agent/src/core/export-html/tool-renderer.ts`
//! (`ToolHtmlRenderer`, `:26-37`) and
//! `packages/coding-agent/src/core/export-html/index.ts` (`ToolHtmlRenderer`,
//! `:15-26`). The port carries the interface only: upstream's
//! `createToolHtmlRenderer` (`tool-renderer.ts:57-175`) invokes extension
//! tool renderers, whose TUI components cannot cross the E1 headless
//! extension boundary (see `docs/EXTENSIONS.md` E2). Until that plumbing
//! lands, custom tools fall back to the template's structured result
//! rendering — `ExportOptions.tool_renderer` stays `None` at both upstream
//! call sites in this port, so `preRenderCustomTools` never renders anything.

use serde_json::Value;

use pi_ai::types::InputContent;

/// One rendered tool result: the collapsed and expanded HTML halves
/// (`{ collapsed?, expanded? }`, `tool-renderer.ts:29-37`). `collapsed` is
/// absent when it equals `expanded`, exactly like the upstream object spread.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenderedToolResult {
    pub collapsed: Option<String>,
    pub expanded: Option<String>,
}

/// `ToolHtmlRenderer` (`tool-renderer.ts:26-37`): render a custom tool call
/// and result to HTML. Returning `None` means the tool has no custom
/// renderer and the template renders the structured data.
pub trait ToolHtmlRenderer {
    /// Render a tool call to HTML. Returns `None` if the tool has no custom
    /// renderer.
    fn render_call(&self, tool_call_id: &str, tool_name: &str, args: &Value) -> Option<String>;
    /// Render a tool result to collapsed/expanded HTML. Returns `None` if the
    /// tool has no custom renderer.
    fn render_result(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        result: &[InputContent],
        details: Option<&Value>,
        is_error: bool,
    ) -> Option<RenderedToolResult>;
}
