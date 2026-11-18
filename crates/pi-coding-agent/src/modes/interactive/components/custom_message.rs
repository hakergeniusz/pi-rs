//! `CustomMessageComponent` — the port of
//! `modes/interactive/components/custom-message.ts:14-138`; the adopted
//! component rides the shared handle proxy
//! ([`super::component_proxy::ComponentProxy`], `tui.ts:124-155`).
//!
//! The renderer (when an extension registered one for the message's
//! `customType`) runs inside the `QuickJS` engine and its component is
//! adopted behind the wrapper's leading `Spacer(1)`. Everything else —
//! no renderer, a renderer returning nothing, a renderer that throws —
//! falls through to the default rendering: the `customMessageBg` box with
//! the bold `[customType]` label and the content as Markdown on the
//! `custom` transformer surface (`custom-message.ts:71-137`; the throw is
//! swallowed silently upstream, `:93-95` — NOT an error box like the
//! custom *entries*).

use std::sync::{Arc, Mutex};

use pi_ai::types::{InputContent, UserContent};
use pi_tui::component::Component;
use pi_tui::components::box_component::BoxComponent;
use pi_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownOptions};

use super::super::theme;
use super::markdown_transform::MarkdownTransformCtx;

/// The chat row for one `custom` message. Upstream extends `Container`
/// with a fixed leading `Spacer(1)`; the port rebuilds the same child
/// stack — spacer + the adopted component, or spacer + the default box.
pub struct CustomMessageComponent {
    container: BoxComponent,
    /// `(runner, extension index)` when an extension registered a renderer
    /// for this message's type — `getMessageRenderer` resolved at the mode
    /// (`interactive-mode.ts:4034`). `None` renders the default box only.
    renderer: Option<(Arc<Mutex<pi_ext_js::ExtensionRunner>>, usize)>,
    custom_type: String,
    /// The message record with its `role:"custom"` — what the renderer
    /// receives (`core/messages.ts:46-54`; the port's struct has no role
    /// field, so the mode's serializer injects it).
    message_json: String,
    /// The text extraction the default box renders
    /// (`custom-message.ts:109-117`): string content as-is, block content
    /// as the text blocks joined with newlines.
    content_text: String,
    expanded: bool,
    output_pad: usize,
    transform_ctx: MarkdownTransformCtx,
    /// Persisted-entry meta, crossing to the markdown transformers
    /// (`renderSessionItems`, `interactive-mode.ts:4241-4246`; undefined
    /// for live messages).
    message_id: Option<String>,
    timestamp: Option<String>,
}

impl CustomMessageComponent {
    /// `constructor(message, renderer, markdownTheme, outputPad,
    /// markdownTransformers, messageId, timestamp)`
    /// (`custom-message.ts:26-50`): the port serializes the message and
    /// resolves the text here, where upstream holds the live object.
    pub fn new(
        renderer: Option<(Arc<Mutex<pi_ext_js::ExtensionRunner>>, usize)>,
        message: &pi_agent::CustomMessage,
        expanded: bool,
        output_pad: usize,
        transform_ctx: MarkdownTransformCtx,
        message_id: Option<String>,
        timestamp: Option<String>,
    ) -> Self {
        let mut value = serde_json::to_value(message).expect("custom message serializes");
        if let Some(object) = value.as_object_mut() {
            object.insert("role".to_string(), serde_json::Value::from("custom"));
        }
        let mut component = Self {
            container: BoxComponent::new(0, 0),
            renderer,
            custom_type: message.custom_type.clone(),
            message_json: serde_json::to_string(&value).expect("custom message serializes"),
            content_text: extract_text(&message.content),
            expanded,
            output_pad,
            transform_ctx,
            message_id,
            timestamp,
        };
        component.rebuild();
        component
    }

    /// `setExpanded` — a changed flag re-renders (`custom-message.ts:52-57`).
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild();
        }
    }

    /// `setOutputPad` — a changed value re-renders
    /// (`custom-message.ts:59-64`).
    pub fn set_output_pad(&mut self, output_pad: usize) {
        if self.output_pad != output_pad {
            self.output_pad = output_pad;
            self.rebuild();
        }
    }

    /// `rebuild` (`custom-message.ts:71-137`): the renderer first — a
    /// component replaces the content — and every other outcome (no
    /// renderer, `undefined`, a throw, a host-level engine failure) falls
    /// through to the default box. The runner lock is dropped before the
    /// rebuilt container replaces the old one: dropping the previous proxy
    /// releases its JS handle, and the release re-locks the runner.
    fn rebuild(&mut self) {
        let mut container = BoxComponent::new(0, 0);
        // The fixed leading spacer rides every rebuild
        // (`custom-message.ts:44`).
        container.add_child(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        let mut rendered = false;

        if let Some((runner, ext_index)) = &self.renderer {
            let options = serde_json::json!({
                "expanded": self.expanded,
                "outputPad": self.output_pad,
            })
            .to_string();
            let outcome = {
                let runner = runner.lock().expect("extension runner mutex poisoned");
                runner.render_message(*ext_index, &self.custom_type, &self.message_json, &options)
            };
            // The component arm replaces the content; everything else — a
            // renderer throw (upstream's catch is silent,
            // `custom-message.ts:93-95`), a renderer returning nothing, and
            // — the port's addition — a host-level engine failure — falls
            // through to the default rendering.
            if let Ok(pi_ext_js::EntryRenderOutcome::Component { handle, has_input }) = outcome {
                let proxy = super::component_proxy::ComponentProxy::new(
                    Arc::clone(runner),
                    handle,
                    has_input,
                );
                container.add_child(Box::new(proxy));
                rendered = true;
            }
        }
        if !rendered {
            container.add_child(Box::new(self.build_default_box()));
        }
        self.container = container;
    }

    /// The default box (`custom-message.ts:98-136`): `customMessageBg`
    /// with the output pad, the bold `[customType]` label in
    /// `customMessageLabel`, a spacer, and the content as Markdown styled
    /// `customMessageText` on the `custom` transformer surface.
    fn build_default_box(&self) -> BoxComponent {
        let theme = theme::theme();
        // The bg closure must be 'static — it re-resolves the theme per
        // call, like the entry error box does.
        let mut box_component = BoxComponent::with_bg(self.output_pad, 1, |text| {
            theme::theme().bg(pi_tui::theme::ThemeBg::CustomMessageBg, text)
        });
        let label = theme.fg(
            pi_tui::theme::ThemeColor::CustomMessageLabel,
            &format!("\x1b[1m[{}]\x1b[22m", self.custom_type),
        );
        box_component.add_child(Box::new(pi_tui::components::text::Text::new(label, 0, 0)));
        box_component.add_child(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        let transform = self.transform_ctx.make(
            "custom",
            None,
            self.message_id.clone(),
            self.timestamp.clone(),
        );
        let style = DefaultTextStyle {
            color: Some(theme.fg_styler(pi_tui::theme::ThemeColor::CustomMessageText)),
            ..DefaultTextStyle::default()
        };
        box_component.add_child(Box::new(Markdown::new(
            self.content_text.clone(),
            0,
            0,
            theme::markdown_theme(),
            Some(style),
            MarkdownOptions {
                preserve_list_markers: true,
                preserve_backslash_escapes: true,
                transform,
            },
        )));
        box_component
    }
}

/// The default-box text extraction (`custom-message.ts:109-117`): string
/// content as-is; block content keeps the text blocks, images dropped.
fn extract_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                InputContent::Text(text) => Some(text.text.clone()),
                InputContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

impl Component for CustomMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    /// Upstream's `override invalidate()` re-runs `rebuild`, so a theme
    /// change re-renders the row with the new mapping
    /// (`custom-message.ts:66-69`).
    fn invalidate(&mut self) {
        self.rebuild();
    }

    /// The mode's expand/output-pad toggles duck-type every chat row with
    /// a setter upstream (`interactive-mode.ts:4753-4758`, `:5270-5281`);
    /// the port reaches them through the tree downcast.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}
