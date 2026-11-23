//! `ToolExecutionComponent` — one tool call row: a bordered box on the
//! pending/success/error background holding the themed call line and result
//! body.
//!
//! Ground truth: `modes/interactive/components/tool-execution.ts`. The
//! upstream component delegates drawing to the tool's `renderCall`/
//! `renderResult` functions, which return Components that are mutated in
//! place across frames (kept via `ToolRenderContext.lastComponent`). Two
//! renderer sources feed the slots: the port's built-in renderers are pure
//! styled-text functions (ported in P4 — one `Text` per slot, rebuilt on
//! updates, the rendered bytes the same), and resolver-supplied extension
//! renderers cross the handle bridge as adopted components
//! ([`super::component_proxy::ComponentProxy`]) with builtin/absent slots
//! falling back to the text paths (`tool-execution.ts:299-344`).
//!
//! Image blocks of a result render inline after the box
//! (`tool-execution.ts:283-336,364-389`): a `Spacer(1)` then an [`Image`]
//! per block, with pi's `toolOutput` fallback color and `imageWidthCells`
//! cap. Sources are compared before rebuilding so an unchanged image keeps
//! its Kitty id/cached lines across result updates. The `showImages` flag
//! (`tool-execution.ts:122-144`, fed by `onShowImagesChange` at
//! `interactive-mode.ts:5153-5160`) is read from a shared live
//! [`ImageSettings`] cell, so a mid-session settings change reaches every
//! row on its next frame.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use pi_tui::component::Component;
use pi_tui::components::box_component::BoxComponent;
use pi_tui::components::image::{Image, ImageOptions, ImageTheme};
use pi_tui::components::mouse_region::MouseRegion;
use pi_tui::components::text::Text;
use pi_tui::mouse::{MouseButton, MouseEvent, MouseEventKind, MouseEventResult};

use crate::images::ImageSettings;
use crate::tools::renderers::ToolRenderers;

use super::super::theme;

/// The extension renderer bridge for one row: the stashed
/// `ToolRenderers` object ([`pi_ext_js::ResolvedToolRenderers`]) resolved
/// at row construction (`getRegisteredToolDefinition`,
/// `interactive-mode.ts:2356-2360`). Inert without the extensions feature.
#[cfg(feature = "ext-js")]
pub struct JsToolRenderers {
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    stash_id: u32,
    /// `renderShell === "self"` (`types.ts:616`): the row draws without the
    /// box framing.
    pub(crate) render_shell_self: bool,
    /// The `renderCall` slot is a resolver-supplied fn.
    pub(crate) render_call_custom: bool,
    /// The `renderResult` slot is a resolver-supplied fn.
    pub(crate) render_result_custom: bool,
}

#[cfg(feature = "ext-js")]
impl JsToolRenderers {
    pub(crate) fn new(
        runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
        resolved: &pi_ext_js::ResolvedToolRenderers,
    ) -> Self {
        Self {
            runner,
            stash_id: resolved.stash_id,
            render_shell_self: resolved.render_shell_self,
            render_call_custom: resolved.render_call == pi_ext_js::ToolRenderSlot::Custom,
            render_result_custom: resolved.render_result == pi_ext_js::ToolRenderSlot::Custom,
        }
    }

    fn render_call(
        &self,
        args_json: &str,
        context_json: &str,
    ) -> Result<pi_ext_js::EntryRenderOutcome, pi_ext_js::JsHostError> {
        self.runner
            .lock()
            .expect("extension runner mutex poisoned")
            .tool_render_call(self.stash_id, args_json, context_json)
    }

    fn render_result(
        &self,
        args_json: &str,
        result_json: &str,
        options_json: &str,
        context_json: &str,
    ) -> Result<pi_ext_js::EntryRenderOutcome, pi_ext_js::JsHostError> {
        self.runner
            .lock()
            .expect("extension runner mutex poisoned")
            .tool_render_result(
                self.stash_id,
                args_json,
                result_json,
                options_json,
                context_json,
            )
    }

    fn release(&self) {
        let _ = self
            .runner
            .lock()
            .expect("extension runner mutex poisoned")
            .tool_renderer_release(self.stash_id);
    }
}

/// The feature-off shape: the mode always passes `None`, the field type
/// still exists so the component's signatures stay identical.
#[cfg(not(feature = "ext-js"))]
#[derive(Clone, Copy)]
pub struct JsToolRenderers;

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

/// One image block of a tool result (`tool-execution.ts:366-383`):
/// base64 bytes plus the mime type the `Image` component needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultImage {
    pub data: String,
    pub mime_type: String,
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
    result: Option<ToolResultView>,
    started_at: Option<Instant>,
    renderers: Option<ToolRenderers>,
    /// Resolver-supplied renderers (`getRegisteredToolDefinition`); `None`
    /// keeps the row on the built-in/generic paths.
    js: Option<JsToolRenderers>,
    /// `renderShell === "self"`: draw without the box framing
    /// (`tool-execution.ts:89-101`).
    render_shell_self: bool,
    /// The rendered box; rebuilt on every display update.
    content_box: BoxComponent,
    /// The expansion a region click asked for; the closure cannot borrow the
    /// component, so `handle_mouse` reconciles it with `self.expanded`.
    click_toggle: Arc<AtomicBool>,
    /// The mode's live image-settings cell (`interactive-mode.ts:5153-5160`).
    /// `showImages` is read per render instead of being frozen at mount, so a
    /// `ShowImagesChange` reaches an already-open row.
    image_settings: Arc<Mutex<ImageSettings>>,
    /// Image blocks of the current result (`tool-execution.ts:366-389`).
    image_sources: Vec<ToolResultImage>,
    /// The components built from `image_sources`, paired with the source
    /// they were built from; unchanged pairs are reused across updates.
    image_components: Vec<(ToolResultImage, Image)>,
}

impl ToolExecutionComponent {
    pub fn new(
        tool_name: &str,
        tool_call_id: &str,
        args: serde_json::Value,
        output_pad: usize,
        cwd: &str,
        renderers: Option<ToolRenderers>,
        js: Option<JsToolRenderers>,
    ) -> Self {
        Self::with_image_settings(
            tool_name,
            tool_call_id,
            args,
            output_pad,
            cwd,
            renderers,
            js,
            Arc::new(Mutex::new(ImageSettings::default())),
        )
    }

    /// The host-facing constructor: the mode threads its shared image-settings
    /// cell (`ImageSettings::from_settings` refreshed on every image setting
    /// change) so the `showImages` flag is live for this row's render context.
    #[must_use]
    #[allow(clippy::too_many_arguments)] // builder mirrors upstream's option struct
    pub fn with_image_settings(
        tool_name: &str,
        tool_call_id: &str,
        args: serde_json::Value,
        output_pad: usize,
        cwd: &str,
        renderers: Option<ToolRenderers>,
        js: Option<JsToolRenderers>,
        image_settings: Arc<Mutex<ImageSettings>>,
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
            result: None,
            started_at: None,
            renderers,
            #[cfg(feature = "ext-js")]
            render_shell_self: js.as_ref().is_some_and(|js| js.render_shell_self),
            #[cfg(not(feature = "ext-js"))]
            render_shell_self: false,
            js,
            content_box: BoxComponent::new(1, 1),
            click_toggle: Arc::new(AtomicBool::new(false)),
            image_settings,
            image_sources: Vec::new(),
            image_components: Vec::new(),
        };
        component.update_display();
        component
    }

    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    /// `updateArgs` (`tool-execution.ts:178-182`). Upstream defers the
    /// rebuild to the next render (`render` applies `argsPending`,
    /// `tool-execution.ts:237`); the port's `render` is immutable, so the
    /// rebuild happens here — one per args delta instead of one per frame.
    pub fn update_args(&mut self, args: serde_json::Value) {
        self.args = args;
        self.update_display();
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

    /// The result's image blocks (`tool-execution.ts:366-383`): only blocks
    /// carrying both base64 data and a mime type are displayed, matching
    /// upstream's `img.data && img.mimeType` guard.
    pub fn update_images(&mut self, images: Vec<ToolResultImage>) {
        self.image_sources = images
            .into_iter()
            .filter(|image| !image.data.is_empty() && !image.mime_type.is_empty())
            .collect();
        self.rebuild_images();
    }

    /// `setShowImages`/`setImageWidthCells` sweep
    /// (`interactive-mode.ts:5153-5168`): the live cell has changed, so
    /// rebuild this row's image components from the current snapshot.
    pub fn refresh_images(&mut self) {
        self.rebuild_images();
    }

    /// Build one [`Image`] per source, reusing the previous component (and
    /// therefore its Kitty id and cached lines) when the source is unchanged
    /// (`tool-execution.ts:370-388`).
    fn rebuild_images(&mut self) {
        let settings = self.image_settings.lock().expect("image settings");
        let width_cells = settings.image_width_cells.max(1) as usize;
        let previous: Vec<Option<_>> = std::mem::take(&mut self.image_components)
            .into_iter()
            .map(Some)
            .collect();
        let mut previous = previous;
        self.image_components = self
            .image_sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                if previous
                    .get(index)
                    .and_then(Option::as_ref)
                    .is_some_and(|(old_source, _)| old_source == source)
                {
                    // Reuse the component so its Kitty id and cached lines
                    // survive an unrelated result update.
                    return previous[index].take().expect("reused image");
                }
                let theme = theme::theme();
                let image = Image::new(
                    source.data.clone(),
                    source.mime_type.clone(),
                    ImageTheme {
                        fallback_color: Box::new(move |text: &str| {
                            theme.fg(pi_tui::theme::ThemeColor::ToolOutput, text)
                        }),
                    },
                    ImageOptions {
                        max_width_cells: Some(width_cells),
                        ..ImageOptions::default()
                    },
                    None,
                );
                (source.clone(), image)
            })
            .collect();
    }

    /// `setExpanded` (`tool-execution.ts:211-214`).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    /// `setOutputPad` (`tool-execution.ts:216-219`); the next render rebuilds
    /// the content box from the field.
    pub fn set_output_pad(&mut self, output_pad: usize) {
        self.output_pad = output_pad;
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
    /// the state's background, the call line, and the result body — a
    /// resolver-supplied component replaces the text child when its slot
    /// is custom and the bridge succeeds, everything else taking the text
    /// paths (`tool-execution.ts:299-344`).
    #[allow(clippy::too_many_lines)] // mirrors the TS updateDisplay; split when extending
    fn update_display(&mut self) {
        let theme = theme::theme();

        let context = RenderContext {
            cwd: &self.cwd,
            expanded: self.expanded,
            is_partial: self.is_partial,
            is_error: self.result.as_ref().is_some_and(|result| result.is_error),
            duration_ms: self.result.as_ref().and_then(|result| result.duration_ms),
            execution_started: self.execution_started,
            started_at: self.started_at,
        };

        // The bridge inputs: the row's args plus the ToolRenderContext
        // flags (the glue adds state/lastComponent/invalidate/keyText).
        #[cfg(feature = "ext-js")]
        let args_json = serde_json::to_string(&self.args).unwrap_or_else(|_| "{}".to_string());
        #[cfg(feature = "ext-js")]
        let context_json = self.build_context_json();

        // The call slot (`tool-execution.ts:299-314`): a custom renderer's
        // component; a throw takes the generic line like upstream's catch —
        // never the tool's built-in renderer (`createCallFallback`,
        // `tool-execution.ts:146-148`).
        #[cfg_attr(not(feature = "ext-js"), allow(unused_mut))]
        let mut call_custom_failed = false;
        #[cfg(feature = "ext-js")]
        let call_custom = match self.js.as_ref().filter(|js| js.render_call_custom) {
            Some(js) => {
                if let Ok(pi_ext_js::EntryRenderOutcome::Component { handle, has_input }) =
                    js.render_call(&args_json, &context_json)
                {
                    Some(Box::new(super::component_proxy::ComponentProxy::new(
                        Arc::clone(&js.runner),
                        handle,
                        has_input,
                    )) as Box<dyn Component>)
                } else {
                    call_custom_failed = true;
                    None
                }
            }
            None => None,
        };
        #[cfg(not(feature = "ext-js"))]
        let call_custom: Option<Box<dyn Component>> = None;

        let call_text = if call_custom_failed {
            crate::tools::renderers::format_tool_call_with_args(
                &self.tool_name,
                Some(&self.args),
                &theme,
                self.expanded,
            )
        } else {
            match self.renderers {
                Some(renderers) => {
                    let call_context = crate::tools::renderers::CallContext {
                        theme: &theme,
                        cwd: &self.cwd,
                        expanded: self.expanded,
                    };
                    (renderers.render_call)(Some(&self.args), &call_context)
                }
                None => crate::tools::renderers::format_tool_call_with_args(
                    &self.tool_name,
                    Some(&self.args),
                    &theme,
                    self.expanded,
                ),
            }
        };

        // The result slot (`tool-execution.ts:316-344`): a custom
        // renderer's component over the `{content, details}` record and the
        // `{expanded, isPartial}` options; a throw takes the sliced generic
        // fallback like upstream's catch — never the tool's built-in
        // renderer (`createResultFallback`, `tool-execution.ts:150-164`).
        #[cfg_attr(not(feature = "ext-js"), allow(unused_mut))]
        let mut result_custom_failed = false;
        #[cfg(feature = "ext-js")]
        let result_custom = match self.js.as_ref().filter(|js| js.render_result_custom) {
            Some(js) => match self.result.as_ref() {
                Some(result) => {
                    let result_json = serde_json::json!({
                        "content": [{ "type": "text", "text": result.text }],
                        "details": serde_json::Value::Null,
                    })
                    .to_string();
                    let options_json = serde_json::json!({
                        "expanded": self.expanded,
                        "isPartial": self.is_partial,
                    })
                    .to_string();
                    if let Ok(pi_ext_js::EntryRenderOutcome::Component { handle, has_input }) =
                        js.render_result(&args_json, &result_json, &options_json, &context_json)
                    {
                        Some(Box::new(super::component_proxy::ComponentProxy::new(
                            Arc::clone(&js.runner),
                            handle,
                            has_input,
                        )) as Box<dyn Component>)
                    } else {
                        result_custom_failed = true;
                        None
                    }
                }
                None => None,
            },
            None => None,
        };
        #[cfg(not(feature = "ext-js"))]
        let result_custom: Option<Box<dyn Component>> = None;

        // The text paths: the tool's Rust renderer, or — resolver rows
        // without the hook — the sliced generic fallback.
        let result_text = if result_custom_failed {
            self.sliced_result_fallback(&theme).unwrap_or_default()
        } else if result_custom.is_none() {
            match &self.result {
                Some(result) => match self.renderers {
                    Some(renderers) => {
                        self.builtin_result_text(&theme, &context, result, renderers)
                    }
                    None if self.js.is_some() => {
                        self.sliced_result_fallback(&theme).unwrap_or_default()
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
                },
                None => String::new(),
            }
        } else {
            String::new()
        };

        let mut content_box = if self.render_shell_self {
            // `renderShell: "self"` rows draw their own framing
            // (`tool-execution.ts:89-101`) — a plain container, no bg or
            // padding.
            BoxComponent::new(0, 0)
        } else {
            let bg_token = if self.is_partial {
                pi_tui::theme::ThemeBg::ToolPendingBg
            } else if self.result.as_ref().is_some_and(|result| result.is_error) {
                pi_tui::theme::ThemeBg::ToolErrorBg
            } else {
                pi_tui::theme::ThemeBg::ToolSuccessBg
            };
            let bg = theme.bg_styler(bg_token);
            BoxComponent::with_bg(self.output_pad.max(1), 1, move |text: &str| bg(text))
        };
        // Every displayed child rides a `MouseRegion` whose left click
        // toggles the row's expansion (`tool-execution.ts:116-124`; the
        // wrapping covers the call at `:228`/`:240` and the result at
        // `:235`/`:249`/`:257`/`:264`).
        let click_toggle = Arc::clone(&self.click_toggle);
        let call_child: Box<dyn Component> = match call_custom {
            Some(component) => component,
            None => Box::new(Text::new(call_text, 0, 0)),
        };
        content_box.add_child(Box::new(MouseRegion::new(
            call_child,
            Box::new(move |event| toggle_on_left_click(event, &click_toggle)),
        )));
        let result_child: Option<Box<dyn Component>> = match result_custom {
            Some(component) => Some(component),
            None if !result_text.is_empty() => Some(Box::new(Text::new(result_text, 0, 0))),
            None => None,
        };
        if let Some(result_child) = result_child {
            let click_toggle = Arc::clone(&self.click_toggle);
            content_box.add_child(Box::new(MouseRegion::new(
                result_child,
                Box::new(move |event| toggle_on_left_click(event, &click_toggle)),
            )));
        }
        self.content_box = content_box;
    }

    /// The `ToolRenderContext` flags the glue extends with state,
    /// lastComponent, invalidate, and the keybinding hints
    /// (`tool-execution.ts:122-144`). `durationMs` stays absent while
    /// partial (absent, not null); `showImages` mirrors the live setting
    /// (`interactive-mode.ts:5153-5160`) instead of a frozen false.
    fn build_context_json(&self) -> String {
        let mut flags = serde_json::Map::new();
        flags.insert(
            "toolCallId".to_string(),
            serde_json::Value::from(self.tool_call_id.as_str()),
        );
        flags.insert(
            "cwd".to_string(),
            serde_json::Value::from(self.cwd.as_str()),
        );
        flags.insert(
            "executionStarted".to_string(),
            serde_json::Value::from(self.execution_started),
        );
        flags.insert(
            "argsComplete".to_string(),
            serde_json::Value::from(self.args_complete),
        );
        flags.insert(
            "isPartial".to_string(),
            serde_json::Value::from(self.is_partial),
        );
        flags.insert(
            "expanded".to_string(),
            serde_json::Value::from(self.expanded),
        );
        let show_images = self
            .image_settings
            .lock()
            .expect("image settings")
            .show_images;
        flags.insert(
            "showImages".to_string(),
            serde_json::Value::from(show_images),
        );
        flags.insert(
            "isError".to_string(),
            serde_json::Value::from(self.result.as_ref().is_some_and(|result| result.is_error)),
        );
        if !self.is_partial
            && let Some(ms) = self.result.as_ref().and_then(|result| result.duration_ms)
        {
            flags.insert("durationMs".to_string(), serde_json::Value::from(ms));
        }
        flags.insert(
            "outputPad".to_string(),
            serde_json::Value::from(self.output_pad as u64),
        );
        serde_json::Value::Object(flags).to_string()
    }

    /// The Rust renderer's result body — the renderers take the
    /// `AgentToolResult` shape, rebuilt from the view's text.
    fn builtin_result_text(
        &self,
        theme: &pi_tui::theme::Theme,
        context: &RenderContext<'_>,
        result: &ToolResultView,
        renderers: ToolRenderers,
    ) -> String {
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
            theme,
            cwd: &self.cwd,
            expanded: self.expanded,
            is_error: result.is_error,
            timer_text: context.timer_text(),
        };
        (renderers.render_result)(Some(&self.args), &agent_result, &result_context)
    }

    /// `createResultFallback` (`tool-execution.ts:150-164`): the output
    /// text, sliced to 10 lines when collapsed, with the remaining count
    /// and the expand key hint. `None` without output — the slot renders
    /// nothing.
    fn sliced_result_fallback(&self, theme: &pi_tui::theme::Theme) -> Option<String> {
        const FALLBACK_PREVIEW_LINES: usize = 10;
        let result = self.result.as_ref()?;
        if result.text.is_empty() {
            return None;
        }
        let lines: Vec<&str> = result.text.split('\n').collect();
        let display_len = if self.expanded {
            lines.len()
        } else {
            lines.len().min(FALLBACK_PREVIEW_LINES)
        };
        let mut parts: Vec<String> = lines[..display_len]
            .iter()
            .map(|line| theme.fg(pi_tui::theme::ThemeColor::ToolOutput, line))
            .collect();
        let remaining = lines.len() - display_len;
        if remaining > 0 {
            let key = theme::key_text(theme::app_keys::TOOLS_EXPAND);
            parts.push(format!(
                "{} {}{}{}",
                theme.fg(
                    pi_tui::theme::ThemeColor::Muted,
                    &format!("\n... ({remaining} more lines,")
                ),
                theme.fg(pi_tui::theme::ThemeColor::Dim, &key),
                theme.fg(pi_tui::theme::ThemeColor::Muted, " to expand"),
                theme.fg(pi_tui::theme::ThemeColor::Muted, ")"),
            ));
        }
        Some(parts.join("\n"))
    }

    /// Record the execution clock start (`startToolTimer` semantics).
    pub fn note_started(&mut self) {
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
    }
}

/// The result region's click toggle (`tool-execution.ts:117-123`): a left
/// click flips the shared flag and reports the event handled; everything
/// else passes through.
fn toggle_on_left_click(event: &MouseEvent, flag: &AtomicBool) -> Option<MouseEventResult> {
    if event.kind != MouseEventKind::Click || event.button != MouseButton::Left {
        return None;
    }
    flag.store(!flag.load(Ordering::Relaxed), Ordering::Relaxed);
    Some(MouseEventResult::handled())
}

impl Drop for ToolExecutionComponent {
    fn drop(&mut self) {
        // Release the row's stashed renderers so the JS registry does not
        // leak (`__piToolRendererRelease`); the box's proxies drop after —
        // sequential locks, never held across each other.
        #[cfg(feature = "ext-js")]
        if let Some(js) = &self.js {
            js.release();
        }
    }
}

impl Component for ToolExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        // `tool-execution.ts:87`: the row opens with a blank line.
        let mut lines = vec![String::new()];
        lines.extend(self.content_box.render(width));
        // `tool-execution.ts:375-388`: each image gets a `Spacer(1)` then
        // the component's own rows. `showImages` is read live so a settings
        // change takes effect without rebuilding the row.
        if self
            .image_settings
            .lock()
            .expect("image settings")
            .show_images
        {
            for (_, image) in &self.image_components {
                lines.push(String::new());
                lines.extend(image.render(width));
            }
        }
        lines
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        // The regions' callbacks cannot borrow the component: seed the flag
        // with the current expansion, let a click flip it, then reconcile
        // (`setExpanded` also rebuilds the display).
        self.click_toggle.store(self.expanded, Ordering::Relaxed);
        let result = self.content_box.handle_mouse(event);
        if self.click_toggle.load(Ordering::Relaxed) != self.expanded {
            self.set_expanded(!self.expanded);
        }
        result
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
            None,
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

    /// `updateArgs` refreshes the call line (`tool-execution.ts:237`): a
    /// bash row moves off the empty-command placeholder once the command
    /// streams in.
    #[test]
    fn streamed_args_refresh_the_call_line() {
        let mut component = ToolExecutionComponent::new(
            "bash",
            "call-1",
            serde_json::json!({ "command": "" }),
            1,
            "/tmp",
            crate::tools::renderers::renderers_for("bash"),
            None,
        );
        let lines = plain(&component, 60);
        assert!(
            lines.iter().any(|line| line.contains("$ ...")),
            "lines: {lines:?}"
        );
        component.update_args(serde_json::json!({ "command": "echo hi" }));
        let lines = plain(&component, 60);
        assert!(
            lines.iter().any(|line| line.contains("$ echo hi")),
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
            None,
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

    /// A result's image blocks render inline (`tool-execution.ts:366-389`),
    /// after a spacer, and respect the live `showImages` flag.
    #[test]
    fn result_images_render_below_the_box_and_follow_show_images() {
        // 1x1 PNG, the smallest payload `Image::new` can sniff.
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";
        let mut component = ToolExecutionComponent::new(
            "read",
            "call-1",
            serde_json::json!({ "path": "/tmp/pic.png" }),
            1,
            "/tmp",
            None,
            None,
        );
        component.update_result(
            ToolResultView {
                text: "done".to_string(),
                is_error: false,
                duration_ms: None,
            },
            false,
        );
        let baseline = plain(&component, 60).len();
        component.update_images(vec![ToolResultImage {
            data: PNG.to_string(),
            mime_type: "image/png".to_string(),
        }]);
        let lines = plain(&component, 60);
        assert!(
            lines.len() >= baseline + 2,
            "spacer + image rows were added: {lines:?}"
        );
        let raw = component.render(60).join("\n");
        assert!(
            raw.contains("[Image: image/png") || raw.contains("\x1b_G") || raw.contains("\x1b]_"),
            "an image (or its fallback) rendered: {raw:?}"
        );

        component
            .image_settings
            .lock()
            .expect("image settings")
            .show_images = false;
        assert_eq!(
            plain(&component, 60).len(),
            baseline,
            "showImages=false hides the inline rows"
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

    #[test]
    fn a_left_click_on_the_row_toggles_expansion() {
        let mut component = ToolExecutionComponent::new(
            "read",
            "call-1",
            serde_json::json!({ "path": "/tmp/example.txt" }),
            1,
            "/tmp",
            crate::tools::renderers::renderers_for("read"),
            None,
        );
        assert!(!component.expanded());
        // The box pads one line; the first content row is the call region.
        let click = MouseEvent::new(MouseEventKind::Click, 0, 2, 1, 60, 3);
        component.handle_mouse(&click).expect("handled");
        assert!(component.expanded(), "a left click expanded the row");
        component.handle_mouse(&click).expect("handled");
        assert!(!component.expanded(), "a second click collapsed it");
        // Other buttons and kinds pass through untouched.
        let middle = MouseEvent::new(MouseEventKind::Click, 1, 2, 1, 60, 3);
        assert!(component.handle_mouse(&middle).is_none());
        let press = MouseEvent::new(MouseEventKind::Press, 0, 2, 1, 60, 3);
        assert!(component.handle_mouse(&press).is_none());
        assert!(!component.expanded());
    }

    /// `onShowImagesChange` (`interactive-mode.ts:5153-5160`) must reach a
    /// row that was mounted before the change: the render context's
    /// `showImages` flag is re-read from the shared settings cell per render.
    #[test]
    fn show_images_flag_tracks_the_live_settings_cell() {
        let live = Arc::new(Mutex::new(ImageSettings {
            show_images: true,
            ..ImageSettings::default()
        }));
        let component = ToolExecutionComponent::with_image_settings(
            "read",
            "call-1",
            serde_json::json!({ "path": "/tmp/example.txt" }),
            1,
            "/tmp",
            crate::tools::renderers::renderers_for("read"),
            None,
            Arc::clone(&live),
        );
        let show_images = || -> bool {
            let context: serde_json::Value =
                serde_json::from_str(&component.build_context_json()).expect("context json");
            context["showImages"].as_bool().expect("showImages bool")
        };
        assert!(show_images(), "the mounted row sees showImages=true");
        live.lock().expect("image settings").show_images = false;
        assert!(!show_images(), "the same row sees the mid-session change");
    }
}
