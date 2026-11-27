//! `BashExecutionComponent` - the dedicated `!` bash row: a `$ command`
//! header, `DynamicBorder` rules, a running spinner and a final status
//! (`(cancelled)` / `(exit N)` / `... N more lines`).
//!
//! Ground truth: `modes/interactive/components/bash-execution.ts:21-223`
//! plus its call sites in `interactive-mode.ts:4010-4027` (resume) and
//! `:7193-7261` (live execution). The component owns the streamed output
//! lines and truncates them for the model context the same way upstream's
//! `truncateTail` call does (`core/tools/truncate.ts`). Pi's `Container`
//! children (spacer, borders, output container) are rendered directly here
//! so the loader can be reused across rebuilds, and the width-aware preview
//! uses the same `Text`-based visual-line folding as
//! `components/visual-truncate.ts:17-52` (there is no port of that utility
//! yet). `!!` rows use the `dim` color token instead of `bashMode`
//! (`bash-execution.ts:42`).

use pi_tui::ansi::strip_terminal_sequences;
use pi_tui::component::Component;
use pi_tui::components::loader::Loader;
use pi_tui::components::text::Text;
use pi_tui::theme::ThemeColor;

use crate::tools::truncate::{TruncationOptions, truncate_tail};

use super::super::theme;
use super::dynamic_border::DynamicBorder;

/// `PREVIEW_LINES` (`bash-execution.ts:16`).
const PREVIEW_LINES: usize = 20;

/// The row's terminal state (`bash-execution.ts:24`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Complete,
    Cancelled,
    Error,
}

/// The color token, chosen by `excludeFromContext` (`bash-execution.ts:42`).
#[derive(Debug, Clone, Copy)]
enum ColorKey {
    BashMode,
    Dim,
}

impl ColorKey {
    fn token(self) -> ThemeColor {
        match self {
            Self::BashMode => ThemeColor::BashMode,
            Self::Dim => ThemeColor::Dim,
        }
    }
}

/// Width-aware collapsed preview (`visual-truncate.ts:17-52`): the tail of
/// the styled output, folded through `Text` so a long wrapped line counts as
/// several visual lines. Upstream keeps only `result.visualLines`; skipped
/// visual lines are not reported here, and the status row carries the logical
/// hidden-line count (`bash-execution.ts:154-158`).
struct VisualLinePreview {
    text: String,
    max_lines: usize,
    padding_x: usize,
    cached: std::cell::RefCell<Option<(usize, Vec<String>)>>,
}

impl VisualLinePreview {
    fn new(text: String, max_lines: usize, padding_x: usize) -> Self {
        Self {
            text,
            max_lines,
            padding_x,
            cached: std::cell::RefCell::new(None),
        }
    }
}

impl Component for VisualLinePreview {
    fn render(&self, width: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = self.cached.borrow().as_ref()
            && *cached_width == width
        {
            return lines.clone();
        }
        let all = Text::new(self.text.clone(), self.padding_x, 0).render(width);
        let lines = if all.len() <= self.max_lines {
            all
        } else {
            // `truncateToVisualLines(...).visualLines`: no hint row.
            all[all.len() - self.max_lines..].to_vec()
        };
        *self.cached.borrow_mut() = Some((width, lines.clone()));
        lines
    }
}

/// One `!` execution row. `bash-execution.ts:21-223`.
pub struct BashExecutionComponent {
    command: String,
    output_lines: Vec<String>,
    status: Status,
    exit_code: Option<i64>,
    loader: Loader,
    expanded: bool,
    output_pad: usize,
    color: ColorKey,
    /// Upstream's result-level truncation flag.
    truncated: bool,
    full_output_path: Option<String>,
    /// The rebuilt header/output/status rows (the loader renders separately
    /// so its frame index survives a status change).
    content: Vec<Box<dyn Component>>,
}

impl BashExecutionComponent {
    /// `constructor(command, ui, excludeFromContext, outputPad)`
    /// (`bash-execution.ts:38-61`). The TUI handle is unnecessary in the
    /// port: the loader is ticked by the host's frame pump instead of an
    /// interval owned by the component.
    #[must_use]
    pub fn new(command: &str, exclude_from_context: bool, output_pad: usize) -> Self {
        let color = if exclude_from_context {
            ColorKey::Dim
        } else {
            ColorKey::BashMode
        };
        let token = color.token();
        let spinner_color = theme::theme().fg_styler(token);
        let message_color = theme::theme().fg_styler(ThemeColor::Muted);
        let loader = Loader::new(
            move |text: &str| spinner_color(text),
            move |text: &str| message_color(text),
            format!(
                "Running... ({} to cancel)",
                theme::key_text("tui.select.cancel")
            ),
            None,
        );
        let mut component = Self {
            command: command.to_string(),
            output_lines: Vec::new(),
            status: Status::Running,
            exit_code: None,
            loader,
            expanded: false,
            output_pad,
            color,
            truncated: false,
            full_output_path: None,
            content: Vec::new(),
        };
        component.update_display();
        component
    }

    /// `setExpanded` (`bash-execution.ts:64-67`).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// `setOutputPad` (`bash-execution.ts:69-72`).
    pub fn set_output_pad(&mut self, output_pad: usize) {
        self.output_pad = output_pad;
        self.update_display();
    }

    /// `appendOutput` (`bash-execution.ts:74-92`): strip ANSI, normalize
    /// line endings, and split a partial last line across chunks.
    pub fn append_output(&mut self, chunk: &str) {
        let clean = strip_terminal_sequences(chunk)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let new_lines: Vec<&str> = clean.split('\n').collect();
        if let Some(last) = self.output_lines.last_mut()
            && let Some(first) = new_lines.first()
        {
            last.push_str(first);
            self.output_lines
                .extend(new_lines.iter().skip(1).map(ToString::to_string));
        } else {
            self.output_lines
                .extend(new_lines.iter().map(ToString::to_string));
        }
        self.update_display();
    }

    /// `setComplete` (`bash-execution.ts:94-120`).
    pub fn set_complete(
        &mut self,
        exit_code: Option<i64>,
        cancelled: bool,
        truncated: bool,
        full_output_path: Option<String>,
    ) {
        self.exit_code = exit_code;
        self.status = if cancelled {
            Status::Cancelled
        } else if exit_code.is_some_and(|code| code != 0) {
            Status::Error
        } else {
            Status::Complete
        };
        self.truncated = truncated;
        self.full_output_path = full_output_path;
        self.loader.stop();
        self.update_display();
    }

    /// The raw output, used when persisting the `BashExecutionMessage`
    /// (`bash-execution.ts:214-216`).
    pub fn output(&self) -> String {
        self.output_lines.join("\n")
    }

    /// Advance the spinner one frame; the host calls this at 80ms.
    pub fn tick_loader(&mut self) -> bool {
        self.loader.tick()
    }

    /// `updateDisplay` (`bash-execution.ts:122-207`).
    #[allow(clippy::too_many_lines)] // mirrors the TS updateDisplay; split when extending
    fn update_display(&mut self) {
        let theme = theme::theme();
        let token = self.color.token();
        let full = self.output_lines.join("\n");
        // Context truncation precedes the preview (`bash-execution.ts:124-134`).
        let context = truncate_tail(&full, TruncationOptions::default());
        let available: Vec<&str> = if context.content.is_empty() {
            Vec::new()
        } else {
            context.content.split('\n').collect()
        };
        let preview_start = available.len().saturating_sub(PREVIEW_LINES);
        let preview = &available[preview_start..];
        let hidden = preview_start;

        let mut rows: Vec<Box<dyn Component>> = Vec::new();
        // `$ command` header (`bash-execution.ts:137`).
        rows.push(Box::new(Text::new(
            theme::bold(&theme.fg(token, &format!("$ {}", self.command))),
            self.output_pad,
            0,
        )));

        if !available.is_empty() {
            if self.expanded {
                let styled: Vec<String> = available
                    .iter()
                    .map(|line| theme.fg(ThemeColor::Muted, line))
                    .collect();
                rows.push(Box::new(Text::new(
                    format!("\n{}", styled.join("\n")),
                    self.output_pad,
                    0,
                )));
            } else {
                let styled: Vec<String> = preview
                    .iter()
                    .map(|line| theme.fg(ThemeColor::Muted, line))
                    .collect();
                rows.push(Box::new(VisualLinePreview::new(
                    format!("\n{}", styled.join("\n")),
                    PREVIEW_LINES,
                    self.output_pad,
                )));
            }
        }

        if self.status != Status::Running {
            let mut parts: Vec<String> = Vec::new();
            if hidden > 0 {
                let key = theme::key_hint(theme::app_keys::TOOLS_EXPAND, "to expand");
                let collapse = theme::key_hint(theme::app_keys::TOOLS_EXPAND, "to collapse");
                let hint = if self.expanded {
                    format!(
                        "{}{}{}",
                        theme.fg(ThemeColor::Muted, "("),
                        collapse,
                        theme.fg(ThemeColor::Muted, ")")
                    )
                } else {
                    format!(
                        "{}{}{}",
                        theme.fg(ThemeColor::Muted, &format!("... {hidden} more lines (")),
                        key,
                        theme.fg(ThemeColor::Muted, ")")
                    )
                };
                parts.push(hint);
            }
            match self.status {
                Status::Cancelled => {
                    parts.push(theme.fg(ThemeColor::Warning, "(cancelled)"));
                }
                Status::Error => {
                    let code = self
                        .exit_code
                        .map_or_else(|| "?".to_string(), |code| code.to_string());
                    parts.push(theme.fg(ThemeColor::Error, &format!("(exit {code})")));
                }
                Status::Running | Status::Complete => {}
            }
            if (self.truncated || context.truncated)
                && let Some(path) = &self.full_output_path
            {
                parts.push(theme.fg(
                    ThemeColor::Warning,
                    &format!("Output truncated. Full output: {path}"),
                ));
            }
            if !parts.is_empty() {
                rows.push(Box::new(Text::new(
                    format!("\n{}", parts.join("\n")),
                    self.output_pad,
                    0,
                )));
            }
        }
        self.content = rows;
    }

    /// The border color callback (`bash-execution.ts:46`).
    fn border(&self) -> DynamicBorder {
        let token = self.color.token();
        DynamicBorder::with_color(theme::theme().fg_styler(token))
    }
}

impl Component for BashExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![String::new()]; // `new Spacer(1)`
        lines.extend(self.border().render(width));
        for child in &self.content {
            lines.extend(child.render(width));
        }
        if self.status == Status::Running {
            lines.extend(self.loader.render(width));
        }
        lines.extend(self.border().render(width));
        lines
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(component: &BashExecutionComponent, width: usize) -> Vec<String> {
        component
            .render(width)
            .iter()
            .map(|line| strip_terminal_sequences(line).trim_end().to_string())
            .collect()
    }

    #[test]
    fn renders_header_borders_and_running_loader() {
        let component = BashExecutionComponent::new("echo hi", false, 1);
        let lines = plain(&component, 50);
        assert_eq!(lines[0], "", "the row opens with a spacer");
        assert!(lines[1].chars().all(|c| c == '─'), "top border: {lines:?}");
        assert!(
            lines.iter().any(|line| line.contains("$ echo hi")),
            "command header: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("Running...")),
            "running loader: {lines:?}"
        );
        assert_eq!(
            lines.last().map(String::as_str),
            Some(lines[1].as_str()),
            "bottom border"
        );
    }

    #[test]
    fn streamed_output_and_final_status_render() {
        let mut component = BashExecutionComponent::new("printf hi", false, 1);
        component.append_output("h");
        component.append_output("i\r\nthere\n");
        assert_eq!(component.output(), "hi\nthere\n");
        component.set_complete(Some(3), false, false, None);
        let lines = plain(&component, 50);
        assert!(lines.iter().any(|line| line.contains("hi")), "{lines:?}");
        assert!(
            lines.iter().any(|line| line.contains("(exit 3)")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|line| line.contains("Running...")));
    }

    #[test]
    fn cancelled_and_hidden_line_hints_render() {
        let mut component = BashExecutionComponent::new("many", true, 1);
        for index in 0..25 {
            component.append_output(&format!("line {index}\n"));
        }
        component.set_complete(None, true, false, None);
        let lines = plain(&component, 80);
        assert!(lines[1].chars().all(|c| c == '─'), "dim border");
        assert!(
            lines.iter().any(|line| line.contains("(cancelled)")),
            "{lines:?}"
        );
        let hints: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains("more lines"))
            .collect();
        assert_eq!(
            hints.len(),
            1,
            "one status hint, no preview hint: {lines:?}"
        );
        assert!(hints[0].contains("6 more lines"), "{lines:?}");
        component.set_expanded(true);
        let expanded = plain(&component, 80);
        assert!(
            expanded.iter().any(|line| line.contains("to collapse")),
            "{expanded:?}"
        );
    }

    /// `truncateToVisualLines` reports skipped visual lines but
    /// `bash-execution.ts` uses only `visualLines`; a single wrapping line must
    /// not produce the old extra `... N more lines` preview row. Upstream's
    /// status count is logical lines hidden, which is zero here.
    #[test]
    fn wrapped_preview_keeps_only_visual_lines_without_a_hint_row() {
        let mut component = BashExecutionComponent::new("wrap", false, 1);
        component.append_output(&format!("{}\n", "x".repeat(400)));
        component.set_complete(Some(0), false, false, None);
        let lines = plain(&component, 20);
        assert!(
            !lines.iter().any(|line| line.contains("more lines")),
            "wrapped visual lines add no hint or status row: {lines:?}"
        );
        // spacer + top border + header + 20 visual preview rows + bottom border
        assert_eq!(
            lines.len(),
            24,
            "exactly PREVIEW_LINES visual rows: {lines:?}"
        );
    }
}
