//! `AssistantMessageComponent` — streaming-aware rendering of an assistant
//! message: text runs as markdown, thinking runs as italic markdown (or a
//! hidden label), stop-reason footers for truncation/abort/error.
//!
//! Ground truth: `modes/interactive/components/assistant-message.ts`. The
//! markdown *slot* system keeps the same `Markdown` instance across updates
//! for the same kind of block at the same position, so a streamed answer
//! reflows instead of re-parsing from scratch. The per-run click-to-toggle
//! (upstream's `MouseRegion`) is deferred with the mouse work; `ctrl+t`
//! toggles all thinking blocks, like upstream's `app.thinking.toggle`.

use pi_tui::component::Component;
use pi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
use pi_tui::components::text::Text;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::super::theme;
use super::markdown_transform::MarkdownTransformCtx;

/// One rendered block. `MarkdownSlot` (`assistant-message.ts:23-26`).
enum Item {
    /// A blank line (upstream's lazy `Spacer(1)`).
    Spacer,
    /// A markdown block whose instance is reused across updates.
    Markdown { kind: SlotKind, markdown: Markdown },
    /// The hidden-thinking label, or a stop-reason footer.
    Text(Text),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Text,
    Thinking,
}

/// The assistant message. `AssistantMessageComponent`
/// (`assistant-message.ts:31-`).
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
            transform_ctx,
        }
    }

    /// `setHideThinkingBlock` (`assistant-message.ts:82-88`).
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.hide_thinking_block = hide;
    }

    /// `setHiddenThinkingLabel` (`assistant-message.ts:90-95`).
    pub fn set_hidden_thinking_label(&mut self, label: &str) {
        self.hidden_thinking_label = label.to_string();
    }

    pub fn output_pad(&self) -> usize {
        self.output_pad
    }

    pub fn set_output_pad(&mut self, pad: usize) {
        self.output_pad = pad;
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
                    thinking_run += 1;
                    if self.hide_thinking_block {
                        if !self.hidden_thinking_label.trim().is_empty() {
                            let theme = theme::theme();
                            push_visible(
                                &mut items,
                                &mut needs_spacer,
                                Item::Text(Text::new(
                                    theme::italic(&theme.fg(
                                        pi_tui::theme::ThemeColor::ThinkingText,
                                        &self.hidden_thinking_label,
                                    )),
                                    output_pad,
                                    0,
                                )),
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
        let _ = thinking_run;

        // Stop-reason footers (`assistant-message.ts:239-257`).
        let has_tool_calls = message
            .content
            .iter()
            .any(|block| matches!(block, pi_ai::types::AssistantContent::ToolCall(_)));
        let theme = theme::theme();
        let error_text = |text: String| {
            Text::new(
                theme.fg(pi_tui::theme::ThemeColor::Error, &text),
                output_pad,
                0,
            )
        };
        match message.stop_reason {
            pi_ai::types::StopReason::Length => {
                items.push(Item::Spacer);
                items.push(Item::Text(error_text(
                    "Response was truncated before completion.".to_string(),
                )));
            }
            pi_ai::types::StopReason::Aborted if !has_tool_calls => {
                let abort_message = message
                    .error_message
                    .as_deref()
                    .filter(|error| *error != "Request was aborted")
                    .unwrap_or("Operation aborted");
                items.push(Item::Spacer);
                items.push(Item::Text(error_text(abort_message.to_string())));
            }
            pi_ai::types::StopReason::Error if !has_tool_calls => {
                let error = message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Unknown error".to_string());
                items.push(Item::Spacer);
                items.push(Item::Text(error_text(format!("Error: {error}"))));
            }
            _ => {}
        }

        self.items = items;
    }
}

impl Component for AssistantMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for item in &self.items {
            match item {
                Item::Spacer => lines.push(String::new()),
                Item::Markdown { markdown, .. } => lines.extend(markdown.render(width)),
                Item::Text(text) => lines.extend(text.render(width)),
            }
        }
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
}
