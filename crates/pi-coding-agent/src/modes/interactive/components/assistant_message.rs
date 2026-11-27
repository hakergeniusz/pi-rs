//! `AssistantMessageComponent` — streaming-aware rendering of an assistant
//! message: text runs as markdown, thinking runs as italic markdown (or a
//! hidden label), stop-reason footers for truncation/abort/error.
//!
//! Ground truth: `modes/interactive/components/assistant-message.ts`. The
//! markdown *slot* system keeps the same `Markdown` instance across updates
//! for the same kind of block at the same position, so a streamed answer
//! reflows instead of re-parsing from scratch. Each thinking run has its own
//! click-to-toggle override (`assistant-message.ts:187-231`), keyed by run
//! index; a click re-renders the stored message with that one run flipped.
//! `ctrl+t` toggles the global `hideThinkingBlock` and clears the overrides,
//! like upstream's `app.thinking.toggle`.
//!
//! Alt-screen prompt markers (`assistant-message.ts:104-115`,
//! `tui-alt-screen.ts:75,515-529`) wrap the rendered lines in OSC 133 zones
//! when the mode enables them; messages with tool calls are skipped, and the
//! main screen never sees the markers (`tui-main-screen.ts:266-269`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_tui::component::Component;
use pi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
use pi_tui::components::text::Text;
use pi_tui::mouse::{MouseButton, MouseEvent, MouseEventKind, MouseEventResult};

use super::super::theme;
use super::markdown_transform::MarkdownTransformCtx;

/// One rendered block. `MarkdownSlot` (`assistant-message.ts:23-26`).
enum Item {
    /// A blank line (upstream's lazy `Spacer(1)`).
    Spacer,
    /// A markdown block whose instance is reused across updates.
    Markdown {
        kind: SlotKind,
        markdown: Markdown,
        /// Set for visible thinking runs, whose `MouseRegion` toggles one run.
        thinking_run: Option<usize>,
    },
    /// The hidden-thinking label, or a stop-reason footer.
    Text {
        text: Text,
        /// Set for hidden thinking runs (the label is clickable too).
        thinking_run: Option<usize>,
    },
}

impl Item {
    fn render(&self, width: usize) -> Vec<String> {
        match self {
            Self::Spacer => vec![String::new()],
            Self::Markdown { markdown, .. } => markdown.render(width),
            Self::Text { text, .. } => text.render(width),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Text,
    Thinking,
}

/// The assistant message. `AssistantMessageComponent`
/// (`assistant-message.ts:31-`).
#[allow(clippy::struct_excessive_bools)] // mirrors the TS component's state flags; split when extending
pub struct AssistantMessageComponent {
    items: Vec<Item>,
    markdown_theme: MarkdownTheme,
    hide_thinking_block: bool,
    hidden_thinking_label: String,
    output_pad: usize,
    streaming: bool,
    /// The shared markdown-transform plumbing (`getMarkdownTransformers`
    /// upstream — the component builds the two surface closures from it).
    transform_ctx: MarkdownTransformCtx,
    /// The live `isStreaming` the transform closures read per render
    /// (`markdown-transform.ts:18` — "read per transform, so a reused
    /// component sees a stream end").
    streaming_flag: Arc<AtomicBool>,
    /// Whether the last update contained tool calls (`hasToolCalls`): such
    /// messages do not mark an OSC 133 prompt zone.
    has_tool_calls: bool,
    /// Alt-screen-only prompt zone markers (see the module doc).
    osc133: bool,
    /// The message a thinking-run click re-renders (`lastMessage`), with the
    /// streaming flag the transform closures need.
    last_message: Option<pi_ai::types::AssistantMessage>,
    last_streaming: bool,
    /// `thinkingVisibilityOverrides`: a click flips one run without touching
    /// the global setting; `setHideThinkingBlock` clears them all.
    thinking_overrides: HashMap<usize, bool>,
    /// `(render width, item heights)` from the last render, so a mouse pass
    /// can find the thinking run under the pointer (`Container.handleMouse`).
    mouse_layout: RefCell<Option<(usize, Vec<usize>)>>,
}

/// Spacers go in lazily, right before the next visible block
/// (`assistant-message.ts:136-143`).
fn push_visible(items: &mut Vec<Item>, needs_spacer: &mut bool, item: Item) {
    if *needs_spacer {
        items.push(Item::Spacer);
        *needs_spacer = false;
    }
    items.push(item);
}

impl AssistantMessageComponent {
    pub fn new(
        hide_thinking_block: bool,
        hidden_thinking_label: &str,
        output_pad: usize,
        transform_ctx: MarkdownTransformCtx,
    ) -> Self {
        Self::with_theme(
            hide_thinking_block,
            hidden_thinking_label,
            output_pad,
            theme::markdown_theme(),
            transform_ctx,
        )
    }

    pub fn with_theme(
        hide_thinking_block: bool,
        hidden_thinking_label: &str,
        output_pad: usize,
        markdown_theme: MarkdownTheme,
        transform_ctx: MarkdownTransformCtx,
    ) -> Self {
        AssistantMessageComponent {
            items: Vec::new(),
            markdown_theme,
            hide_thinking_block,
            hidden_thinking_label: hidden_thinking_label.to_string(),
            output_pad,
            streaming: false,
            streaming_flag: Arc::new(AtomicBool::new(false)),
            has_tool_calls: false,
            osc133: false,
            last_message: None,
            last_streaming: false,
            thinking_overrides: HashMap::new(),
            mouse_layout: RefCell::new(None),
            transform_ctx,
        }
    }

    /// Enable the markers only on the alt screen (`tui-main-screen.ts:268`).
    pub fn set_osc133_markers(&mut self, enabled: bool) {
        self.osc133 = enabled;
    }

    /// `setHideThinkingBlock` (`assistant-message.ts:82-88`): the global
    /// change clears per-run overrides, then rebuilds from the stored message.
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.hide_thinking_block = hide;
        self.thinking_overrides.clear();
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.last_streaming);
        }
    }

    /// `setHiddenThinkingLabel` (`assistant-message.ts:90-95`).
    pub fn set_hidden_thinking_label(&mut self, label: &str) {
        self.hidden_thinking_label = label.to_string();
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.last_streaming);
        }
    }

    pub fn output_pad(&self) -> usize {
        self.output_pad
    }

    pub fn set_output_pad(&mut self, pad: usize) {
        self.output_pad = pad;
        // Markdown takes its padding at construction
        // (`assistant-message.ts:98-101`), so drop the old instances like
        // upstream's `this.markdownSlots = []`.
        self.items.clear();
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.last_streaming);
        }
    }

    /// `updateContent` (`assistant-message.ts:117-258`): rebuild the item
    /// list, reusing markdown slots at the same position for the same kind.
    // The linear port of upstream's update sequence; splitting it would
    // obscure the slot-reuse bookkeeping.
    #[allow(clippy::too_many_lines)]
    pub fn update_content(&mut self, message: &pi_ai::types::AssistantMessage, streaming: bool) {
        self.streaming = streaming;
        self.streaming_flag.store(streaming, Ordering::Relaxed);

        let previous = std::mem::take(&mut self.items);
        // Slot candidates from the previous frame, consumed in order.
        let mut slots = previous.into_iter();
        let mut next_slot =
            |kind: SlotKind, text: &str, build: &dyn Fn() -> Markdown| -> Markdown {
                for item in slots.by_ref() {
                    if let Item::Markdown {
                        kind: item_kind,
                        markdown,
                        ..
                    } = item
                    {
                        if item_kind == kind {
                            let mut markdown = markdown;
                            markdown.set_text(text);
                            return markdown;
                        }
                        // Different kind: this slot is dropped, keep scanning is
                        // wrong for upstream's positional reuse — upstream only
                        // reuses when the *previous slot at this index* matches.
                        // Scanning until a match would shuffle instances across
                        // positions, so stop here and rebuild.
                        break;
                    }
                }
                // Fresh slot: `build` makes the empty instance; the text still
                // has to go in.
                let mut markdown = build();
                markdown.set_text(text);
                markdown
            };

        let mut items: Vec<Item> = Vec::new();
        let markdown_theme = self.markdown_theme.clone();
        let output_pad = self.output_pad;
        let transform_ctx = self.transform_ctx.clone();
        let streaming_flag = Arc::clone(&self.streaming_flag);
        let thinking_style = || {
            let theme = theme::theme();
            DefaultTextStyle {
                color: Some(theme.fg_styler(pi_tui::theme::ThemeColor::ThinkingText)),
                italic: true,
                ..DefaultTextStyle::default()
            }
        };

        // Spacers go in lazily, right before the next visible block
        // (`assistant-message.ts:136-143`).
        let mut needs_spacer = true;

        let content = message.content.clone();
        let mut index = 0;
        let mut thinking_run = 0usize;
        while index < content.len() {
            match &content[index] {
                pi_ai::types::AssistantContent::Text(block) => {
                    let text = block.text.trim().to_string();
                    if !text.is_empty() {
                        let build = || {
                            Markdown::new(
                                "",
                                output_pad,
                                0,
                                markdown_theme.clone(),
                                None,
                                MarkdownOptions {
                                    // The `assistant` surface
                                    // (`assistant-message.ts:159-165`).
                                    transform: transform_ctx.make(
                                        "assistant",
                                        Some(Arc::clone(&streaming_flag)),
                                        None,
                                        None,
                                    ),
                                    ..MarkdownOptions::default()
                                },
                            )
                        };
                        let markdown = next_slot(SlotKind::Text, &text, &build);
                        push_visible(
                            &mut items,
                            &mut needs_spacer,
                            Item::Markdown {
                                kind: SlotKind::Text,
                                markdown,
                                thinking_run: None,
                            },
                        );
                    }
                    index += 1;
                }
                pi_ai::types::AssistantContent::Thinking(_) => {
                    // Group the consecutive thinking blocks into one run
                    // (`assistant-message.ts:169-181`).
                    let mut blocks: Vec<String> = Vec::new();
                    while index < content.len() {
                        match &content[index] {
                            pi_ai::types::AssistantContent::Thinking(thinking) => {
                                let trimmed = thinking.thinking.trim().to_string();
                                if !trimmed.is_empty() {
                                    blocks.push(trimmed);
                                }
                                index += 1;
                            }
                            _ => break,
                        }
                    }
                    if blocks.is_empty() {
                        continue;
                    }
                    let run_index = thinking_run;
                    thinking_run += 1;
                    let hidden = self
                        .thinking_overrides
                        .get(&run_index)
                        .copied()
                        .unwrap_or(self.hide_thinking_block);
                    if hidden {
                        if !self.hidden_thinking_label.trim().is_empty() {
                            let theme = theme::theme();
                            push_visible(
                                &mut items,
                                &mut needs_spacer,
                                Item::Text {
                                    text: Text::new(
                                        theme::italic(&theme.fg(
                                            pi_tui::theme::ThemeColor::ThinkingText,
                                            &self.hidden_thinking_label,
                                        )),
                                        output_pad,
                                        0,
                                    ),
                                    thinking_run: Some(run_index),
                                },
                            );
                        }
                        // A hidden run with an empty label renders nothing.
                    } else {
                        let thinking = blocks.join("\n\n");
                        let build = || {
                            Markdown::new(
                                "",
                                output_pad,
                                0,
                                markdown_theme.clone(),
                                Some(thinking_style()),
                                MarkdownOptions {
                                    // The `assistant-thinking` surface
                                    // (`assistant-message.ts:210-216`).
                                    transform: transform_ctx.make(
                                        "assistant-thinking",
                                        Some(Arc::clone(&streaming_flag)),
                                        None,
                                        None,
                                    ),
                                    ..MarkdownOptions::default()
                                },
                            )
                        };
                        let markdown = next_slot(SlotKind::Thinking, &thinking, &build);
                        push_visible(
                            &mut items,
                            &mut needs_spacer,
                            Item::Markdown {
                                kind: SlotKind::Thinking,
                                markdown,
                                thinking_run: Some(run_index),
                            },
                        );
                    }
                    // Spacing only when another visible block follows
                    // (`assistant-message.ts:228-230`).
                    needs_spacer = true;
                }
                pi_ai::types::AssistantContent::ToolCall(_) => {
                    // Tool calls render as separate tool-execution rows.
                    index += 1;
                }
            }
        }

        // Stop-reason footers (`assistant-message.ts:239-257`).
        let has_tool_calls = message
            .content
            .iter()
            .any(|block| matches!(block, pi_ai::types::AssistantContent::ToolCall(_)));
        let theme = theme::theme();
        let error_text = |text: String| Item::Text {
            text: Text::new(
                theme.fg(pi_tui::theme::ThemeColor::Error, &text),
                output_pad,
                0,
            ),
            thinking_run: None,
        };
        match message.stop_reason {
            pi_ai::types::StopReason::Length => {
                items.push(Item::Spacer);
                items.push(error_text(
                    "Response was truncated before completion.".to_string(),
                ));
            }
            pi_ai::types::StopReason::Aborted if !has_tool_calls => {
                let abort_message = message
                    .error_message
                    .as_deref()
                    .filter(|error| *error != "Request was aborted")
                    .unwrap_or("Operation aborted");
                items.push(Item::Spacer);
                items.push(error_text(abort_message.to_string()));
            }
            pi_ai::types::StopReason::Error if !has_tool_calls => {
                let error = message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Unknown error".to_string());
                items.push(Item::Spacer);
                items.push(error_text(format!("Error: {error}")));
            }
            _ => {}
        }

        self.has_tool_calls = has_tool_calls;
        self.last_message = Some(message.clone());
        self.last_streaming = streaming;
        self.items = items;
    }
}

impl Component for AssistantMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut heights = Vec::with_capacity(self.items.len());
        for item in &self.items {
            let item_lines = item.render(width);
            heights.push(item_lines.len());
            lines.extend(item_lines);
        }
        *self.mouse_layout.borrow_mut() = Some((width, heights));
        if self.osc133 && !self.has_tool_calls && !lines.is_empty() {
            super::user_message::with_osc133_markers(lines)
        } else {
            lines
        }
    }

    /// One thinking run per click (`assistant-message.ts:218-227`): find the
    /// item under the pointer with the cached band heights, flip that run's
    /// override, then rebuild from the stored message like upstream's
    /// `MouseRegion` callback does with `updateContent(lastMessage)`.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        if event.kind != MouseEventKind::Click || event.button != MouseButton::Left {
            return None;
        }
        if event.y < 0 || event.y >= event.height as i64 {
            return None;
        }
        let cached = self.mouse_layout.borrow().clone();
        let heights = match cached {
            Some((cached_width, heights)) if cached_width == event.width => heights,
            _ => self
                .items
                .iter()
                .map(|item| item.render(event.width).len())
                .collect(),
        };
        let mut item_y = 0i64;
        for (item, height) in self.items.iter().zip(heights) {
            if event.y >= item_y && event.y < item_y + height as i64 {
                let run = match item {
                    Item::Text {
                        thinking_run: Some(run),
                        ..
                    }
                    | Item::Markdown {
                        thinking_run: Some(run),
                        ..
                    } => Some(*run),
                    _ => None,
                };
                let run = run?;
                let hidden = self
                    .thinking_overrides
                    .get(&run)
                    .copied()
                    .unwrap_or(self.hide_thinking_block);
                self.thinking_overrides.insert(run, !hidden);
                if let Some(message) = self.last_message.clone() {
                    self.update_content(&message, self.last_streaming);
                }
                return Some(MouseEventResult::handled());
            }
            item_y += height as i64;
        }
        None
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    fn assistant_text_message(text: &str) -> pi_ai::types::AssistantMessage {
        serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
            "api": "test",
            "provider": "test",
            "model": "test",
            "usage": {
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 0,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
            },
            "stopReason": "stop",
            "timestamp": 0
        }))
        .expect("assistant message")
    }

    fn plain(component: &AssistantMessageComponent, width: usize) -> Vec<String> {
        component
            .render(width)
            .iter()
            .map(|line| strip_terminal_sequences(line).trim_end().to_string())
            .collect()
    }

    /// The alt-screen prompt markers skip tool-call messages
    /// (`assistant-message.ts:106-113`).
    #[test]
    fn osc133_markers_skip_tool_call_messages() {
        let mut component = AssistantMessageComponent::new(
            false,
            "Thinking...",
            1,
            MarkdownTransformCtx::default(),
        );
        component.update_content(&assistant_text_message("hello there"), false);
        assert!(!component.render(40).join("\n").contains("\x1b]133;A"));
        component.set_osc133_markers(true);
        assert!(
            component.render(40).join("\n").contains("\x1b]133;A\x07"),
            "a plain message marks its zone"
        );

        let mut with_tool = assistant_text_message("");
        with_tool
            .content
            .push(pi_ai::types::AssistantContent::ToolCall(
                pi_ai::types::ToolCall {
                    id: "c1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::Map::new(),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                },
            ));
        component.update_content(&with_tool, false);
        assert!(
            !component.render(40).join("\n").contains("\x1b]133;A"),
            "a tool-call message has no prompt zone"
        );
    }

    #[test]
    fn renders_text_with_a_leading_spacer() {
        let mut component = AssistantMessageComponent::new(
            false,
            "Thinking...",
            1,
            MarkdownTransformCtx::default(),
        );
        component.update_content(&assistant_text_message("hello there"), false);
        let lines = plain(&component, 40);
        assert_eq!(lines.first().map(String::as_str), Some(""));
        assert!(lines.iter().any(|line| line.contains("hello there")));
    }

    #[test]
    fn thinking_blocks_render_italic_or_hidden() {
        let mut message = assistant_text_message("");
        message.content = vec![
            pi_ai::types::AssistantContent::Thinking(pi_ai::types::ThinkingContent {
                thinking: "pondering".to_string(),
                thinking_signature: None,
                redacted: false,
            }),
            pi_ai::types::AssistantContent::Text(pi_ai::types::TextContent {
                text: "answer".to_string(),
                text_signature: None,
            }),
        ];
        let mut visible = AssistantMessageComponent::new(
            false,
            "Thinking...",
            1,
            MarkdownTransformCtx::default(),
        );
        visible.update_content(&message, false);
        assert!(
            plain(&visible, 40)
                .iter()
                .any(|line| line.contains("pondering"))
        );

        let mut hidden = AssistantMessageComponent::new(
            true,
            "Thought for a bit",
            1,
            MarkdownTransformCtx::default(),
        );
        hidden.update_content(&message, false);
        let lines = plain(&hidden, 40);
        assert!(!lines.iter().any(|line| line.contains("pondering")));
        assert!(lines.iter().any(|line| line.contains("Thought for a bit")));
    }

    #[test]
    fn slots_are_reused_for_the_same_kind() {
        let mut component = AssistantMessageComponent::new(
            false,
            "Thinking...",
            1,
            MarkdownTransformCtx::default(),
        );
        component.update_content(&assistant_text_message("first"), true);
        // Second update with different text keeps the same item shape.
        component.update_content(&assistant_text_message("second"), false);
        let lines = plain(&component, 40);
        assert!(lines.iter().any(|line| line.contains("second")));
        assert!(!lines.iter().any(|line| line.contains("first")));
    }

    /// Upstream's per-run `thinkingVisibilityOverrides`
    /// (`assistant-message.ts:187-231`): a click flips one run, and the global
    /// setting resets every override.
    #[test]
    #[allow(clippy::too_many_lines)] // mirrors the upstream multi-run toggle scenario; split when extending
    fn a_click_toggles_one_thinking_run_at_a_time() {
        let thought = |text: &str| {
            pi_ai::types::AssistantContent::Thinking(pi_ai::types::ThinkingContent {
                thinking: text.to_string(),
                thinking_signature: None,
                redacted: false,
            })
        };
        let text = |text: &str| {
            pi_ai::types::AssistantContent::Text(pi_ai::types::TextContent {
                text: text.to_string(),
                text_signature: None,
            })
        };
        let mut message = assistant_text_message("");
        message.content = vec![
            thought("first thought"),
            text("middle"),
            thought("second thought"),
            text("answer"),
        ];
        let mut component =
            AssistantMessageComponent::new(true, "Thinking...", 1, MarkdownTransformCtx::default());
        component.update_content(&message, false);
        let lines = plain(&component, 60);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("Thinking..."))
                .count(),
            2,
            "both runs start hidden: {lines:?}"
        );
        let first = lines
            .iter()
            .position(|line| line.contains("Thinking..."))
            .expect("first hidden label") as i64;
        component
            .handle_mouse(&MouseEvent::new(
                MouseEventKind::Click,
                0,
                1,
                first,
                60,
                lines.len(),
            ))
            .expect("handled");
        let lines = plain(&component, 60);
        assert!(lines.iter().any(|line| line.contains("first thought")));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("Thinking..."))
                .count(),
            1,
            "only the first run opened: {lines:?}"
        );

        let second = lines
            .iter()
            .position(|line| line.contains("Thinking..."))
            .expect("second hidden label") as i64;
        component
            .handle_mouse(&MouseEvent::new(
                MouseEventKind::Click,
                0,
                1,
                second,
                60,
                lines.len(),
            ))
            .expect("handled");
        let lines = plain(&component, 60);
        assert!(
            lines.iter().any(|line| line.contains("second thought")),
            "the second run opens independently: {lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("first thought")));

        let first = lines
            .iter()
            .position(|line| line.contains("first thought"))
            .expect("first visible thought") as i64;
        component
            .handle_mouse(&MouseEvent::new(
                MouseEventKind::Click,
                0,
                1,
                first,
                60,
                lines.len(),
            ))
            .expect("handled");
        let lines = plain(&component, 60);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("Thinking..."))
                .count(),
            1,
            "the first run closed again: {lines:?}"
        );

        component.set_hide_thinking_block(false);
        let lines = plain(&component, 60);
        assert!(lines.iter().any(|line| line.contains("first thought")));
        assert!(
            lines.iter().any(|line| line.contains("second thought")),
            "a global change clears the overrides: {lines:?}"
        );
    }
}
