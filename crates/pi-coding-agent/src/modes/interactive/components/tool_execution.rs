//! `ToolExecutionComponent` — one tool call row: a bordered box on the
//! pending/success/error background holding the themed call line and result
//! body.
//!
//! Ground truth: `modes/interactive/components/tool-execution.ts`. The
//! upstream component delegates drawing to the tool's `renderCall`/
//! `renderResult` functions, which return Components that are mutated in
//! place across frames (kept via `ToolRenderContext.lastComponent`). This
//! port's renderers are pure styled-text functions (the text shapes were
//! ported in P4), so the component owns one `Text` for the call line and one
//! for the result body, rebuilt on updates — the rendered bytes are the same.
//! The image-block display of results is deferred with the image work.

use std::time::Instant;

use pi_tui::component::Component;
use pi_tui::components::box_component::BoxComponent;
use pi_tui::components::text::Text;

use crate::tools::renderers::ToolRenderers;

use super::super::theme;

/// What the renderers need to know about the row's live state
/// (`ToolRenderContext` reduced to what the built-in renderers read).
// The bools mirror pi's `ToolRenderContext` flags one-to-one.
#[allow(clippy::struct_excessive_bools)]
pub struct RenderContext<'a> {
    pub cwd: &'a str,
    /// pi's `expanded` (ctrl+o).
    pub expanded: bool,
    /// pi's `isPartial`.
    pub is_partial: bool,
    pub is_error: bool,
    /// Execution time of a final result (`ToolRenderContext.durationMs`).
    pub duration_ms: Option<u64>,
    /// pi's `executionStarted`.
    pub execution_started: bool,
    /// `startToolTimer`/`toolTimerText` state: when the clock started.
    pub started_at: Option<Instant>,
}

impl RenderContext<'_> {
    /// `toolTimerText` (`render-utils.ts:146-164`): a final result's recorded
    /// duration wins outright; otherwise `Elapsed 4.2s` while partial and
    /// `Took 4.2s` from the row's own clock, undefined with no start time.
    pub fn timer_text(&self) -> Option<String> {
        if !self.is_partial
            && let Some(ms) = self.duration_ms
        {
            return Some(format!(
                "Took {}",
                format_tool_duration(std::time::Duration::from_millis(ms))
            ));
        }
        let started_at = self.started_at?;
        if self.is_partial {
            return Some(format!(
                "Elapsed {}",
                format_tool_duration(started_at.elapsed())
            ));
        }
        Some(format!(
            "Took {}",
            format_tool_duration(started_at.elapsed())
        ))
    }
}

/// `formatToolDuration` (`render-utils.ts:113-123`): `4.2s` below a minute,
/// then `1m 30s` and `2h 3m 4s`.
pub fn format_tool_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let total_seconds = seconds as u64;
    let minutes = total_seconds / 60;
    let remainder = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {remainder}s");
    }
    format!("{}h {}m {remainder}s", minutes / 60, minutes % 60)
}

/// The final (or partial) result view.
#[derive(Debug, Clone)]
pub struct ToolResultView {
    /// Joined text blocks of the result.
    pub text: String,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
}

/// The row. `ToolExecutionComponent` (`tool-execution.ts:31-`).
// The bools mirror pi's row-state flags one-to-one.
#[allow(clippy::struct_excessive_bools)]
pub struct ToolExecutionComponent {
    tool_name: String,
    tool_call_id: String,
    args: serde_json::Value,
    cwd: String,
    expanded: bool,
    output_pad: usize,
    is_partial: bool,
    execution_started: bool,
    args_complete: bool,
    args_pending: bool,
    result: Option<ToolResultView>,
    started_at: Option<Instant>,
    renderers: Option<ToolRenderers>,
    /// The rendered box; rebuilt on every display update.
    content_box: BoxComponent,
}

impl ToolExecutionComponent {
    pub fn new(
        tool_name: &str,
        tool_call_id: &str,
        args: serde_json::Value,
        output_pad: usize,
        cwd: &str,
        renderers: Option<ToolRenderers>,
    ) -> Self {
        let mut component = ToolExecutionComponent {
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.to_string(),
            args,
            cwd: cwd.to_string(),
            expanded: false,
            output_pad,
            is_partial: true,
            execution_started: false,
            args_complete: false,
            args_pending: false,
            result: None,
            started_at: None,
            renderers,
            content_box: BoxComponent::new(1, 1),
        };
        component.update_display();
        component
    }

    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    /// `updateArgs` (`tool-execution.ts:178-182`): applied at the next render
    /// so a stream of argument deltas runs the renderer once per frame.
    pub fn update_args(&mut self, args: serde_json::Value) {
        self.args = args;
        self.args_pending = true;
    }

    /// `markExecutionStarted` (`tool-execution.ts:184-188`).
    pub fn mark_execution_started(&mut self) {
        self.execution_started = true;
        self.update_display();
    }

    /// `setArgsComplete` (`tool-execution.ts:190-194`).
    pub fn set_args_complete(&mut self) {
        self.args_complete = true;
        self.update_display();
    }

    /// `updateResult` (`tool-execution.ts:196-209`).
    pub fn update_result(&mut self, result: ToolResultView, is_partial: bool) {
        self.result = Some(result);
        self.is_partial = is_partial;
        self.update_display();
    }

    /// `setExpanded` (`tool-execution.ts:211-214`).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// Advance the row's timer (host-driven 1s tick while partial).
    pub fn tick_timer(&mut self) {
        if self.started_at.is_some() && self.is_partial {
            self.update_display();
        }
    }

    /// `updateDisplay` (`tool-execution.ts:281-402`): rebuild the box with
    /// the state's background, the call line, and the result body.
    fn update_display(&mut self) {
        self.args_pending = false;
        let theme = theme::theme();
        let bg_token = if self.is_partial {
            pi_tui::theme::ThemeBg::ToolPendingBg
        } else if self.result.as_ref().is_some_and(|result| result.is_error) {
            pi_tui::theme::ThemeBg::ToolErrorBg
        } else {
            pi_tui::theme::ThemeBg::ToolSuccessBg
        };
        let bg = theme.bg_styler(bg_token);

        let context = RenderContext {
            cwd: &self.cwd,
            expanded: self.expanded,
            is_partial: self.is_partial,
            is_error: self.result.as_ref().is_some_and(|result| result.is_error),
            duration_ms: self.result.as_ref().and_then(|result| result.duration_ms),
            execution_started: self.execution_started,
            started_at: self.started_at,
        };

        // The call line: the tool's renderer, or the generic fallback
        // (`formatToolCallWithArgs`).
        let call_context = crate::tools::renderers::CallContext {
            theme: &theme,
            cwd: &self.cwd,
            expanded: self.expanded,
        };
        let call_text = match self.renderers {
            Some(renderers) => (renderers.render_call)(Some(&self.args), &call_context),
            None => crate::tools::renderers::format_tool_call_with_args(
                &self.tool_name,
                Some(&self.args),
                &theme,
                self.expanded,
            ),
        };

        // The result body. The renderers take the `AgentToolResult` shape,
        // rebuilt from the view's text.
        let result_text = match &self.result {
            Some(result) => {
                let agent_result = pi_agent::types::AgentToolResult {
                    content: vec![pi_ai::types::ContentBlock::Text(
                        pi_ai::types::TextContent {
                            text: result.text.clone(),
                            text_signature: None,
                        },
                    )],
                    details: serde_json::Value::Null,
                    structured_content: None,
                    usage: None,
                    is_error: false,
                    terminate: false,
                };
                let result_context = crate::tools::renderers::ResultContext {
                    theme: &theme,
                    cwd: &self.cwd,
                    expanded: self.expanded,
                    is_error: result.is_error,
                    timer_text: context.timer_text(),
                };
                match self.renderers {
                    Some(renderers) => {
                        (renderers.render_result)(Some(&self.args), &agent_result, &result_context)
                    }
                    None => {
                        if result.text.is_empty() {
                            String::new()
                        } else {
                            result
                                .text
                                .split('\n')
                                .map(|line| theme.fg(pi_tui::theme::ThemeColor::ToolOutput, line))
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                    }
                }
            }
            None => String::new(),
        };

        let bg_fn = move |text: &str| bg(text);
        let mut content_box = BoxComponent::with_bg(self.output_pad.max(1), 1, bg_fn);
        content_box.add_child(Box::new(Text::new(call_text, 0, 0)));
        if !result_text.is_empty() {
            content_box.add_child(Box::new(Text::new(result_text, 0, 0)));
        }
        self.content_box = content_box;
    }

    /// Record the execution clock start (`startToolTimer` semantics).
    pub fn note_started(&mut self) {
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
    }
}

impl Component for ToolExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        // `tool-execution.ts:87`: the row opens with a blank line.
        let mut lines = vec![String::new()];
        lines.extend(self.content_box.render(width));
        lines
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    fn plain(component: &ToolExecutionComponent, width: usize) -> Vec<String> {
        component
            .render(width)
            .iter()
            .map(|line| strip_terminal_sequences(line).trim_end().to_string())
            .collect()
    }

    #[test]
    fn renders_the_call_line_inside_a_pending_box() {
        let component = ToolExecutionComponent::new(
            "read",
            "call-1",
            serde_json::json!({ "path": "/tmp/example.txt" }),
            1,
            "/tmp",
            crate::tools::renderers::renderers_for("read"),
        );
        let lines = plain(&component, 60);
        assert_eq!(lines.first().map(String::as_str), Some(""));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("read") && line.contains("example.txt")),
            "lines: {lines:?}"
        );
    }

    #[test]
    fn result_switches_the_background_and_shows_output() {
        let mut component = ToolExecutionComponent::new(
            "bash",
            "call-1",
            serde_json::json!({ "command": "echo hi" }),
            1,
            "/tmp",
            crate::tools::renderers::renderers_for("bash"),
        );
        component.update_result(
            ToolResultView {
                text: "hi\n".to_string(),
                is_error: false,
                duration_ms: Some(1200),
            },
            false,
        );
        let lines = plain(&component, 60);
        assert!(
            lines.iter().any(|line| line.contains("hi")),
            "lines: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("Took 1.2s")),
            "lines: {lines:?}"
        );
    }

    #[test]
    fn tool_duration_formats_like_upstream() {
        assert_eq!(
            format_tool_duration(std::time::Duration::from_millis(4200)),
            "4.2s"
        );
        assert_eq!(
            format_tool_duration(std::time::Duration::from_secs(90)),
            "1m 30s"
        );
        assert_eq!(
            format_tool_duration(std::time::Duration::from_secs(7384)),
            "2h 3m 4s"
        );
    }
}
