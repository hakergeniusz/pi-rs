//! `CustomEntryComponent` — the port of
//! `modes/interactive/components/custom-entry.ts`; the adopted component
//! rides the shared handle proxy ([`super::component_proxy::ComponentProxy`],
//! `tui.ts:124-155`).
//!
//! Upstream renderers run in-process and return a live `Component`; here
//! the renderer runs inside the `QuickJS` engine, the returned object is
//! adopted into the JS handle registry, and this side holds only the
//! numeric handle: `render` crosses as `{lines}` JSON, `handleInput` as
//! `{consume}`, and `invalidate`/release as ops. The wrapper mirrors
//! upstream's rebuild cycle — a `Spacer(1)` plus the renderer's component,
//! the renderer's throw becoming the host's error box
//! (`custom-entry.ts:44-50`), and a `undefined` render leaving the entry
//! without content so `addCustomEntryToChat` skips it
//! (`interactive-mode.ts:3986-4007`).

use std::sync::{Arc, Mutex};

use pi_tui::component::Component;
use pi_tui::components::box_component::BoxComponent;

use super::super::theme;

/// The chat row for one `custom` session entry. `output_pad` and the
/// expanded flag ride the constructor / [`Self::set_expanded`], like
/// upstream's `outputPad`/`setExpanded`.
pub struct CustomEntryComponent {
    /// The rebuilt child stack: `Spacer(1)` + the adopted component (or the
    /// error box). Upstream extends `Container`.
    container: BoxComponent,
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    ext_index: usize,
    custom_type: String,
    /// The on-disk entry record as JSON — what upstream's renderer receives.
    entry_json: String,
    expanded: bool,
    output_pad: usize,
    has_content: bool,
}

impl CustomEntryComponent {
    /// `constructor(entry, renderer, outputPad = 1)` — the port resolves the
    /// renderer index at the mode (`getEntryRenderer` there), so it rides in.
    pub fn new(
        runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
        ext_index: usize,
        custom_type: &str,
        entry_json: &str,
        expanded: bool,
        output_pad: usize,
    ) -> Self {
        let mut component = Self {
            container: BoxComponent::new(0, 0),
            runner,
            ext_index,
            custom_type: custom_type.to_string(),
            entry_json: entry_json.to_string(),
            expanded,
            output_pad,
            has_content: false,
        };
        component.rebuild();
        component
    }

    /// `hasContent()`: the renderer produced something to show.
    pub fn has_content(&self) -> bool {
        self.has_content
    }

    /// `setExpanded` — a changed flag re-renders the entry.
    pub fn set_expanded(&mut self, expanded: bool) {
        if self.expanded != expanded {
            self.expanded = expanded;
            self.rebuild();
        }
    }

    /// `setOutputPad` — every change re-renders (`custom-entry.ts:38-41`).
    pub fn set_output_pad(&mut self, output_pad: usize) {
        self.output_pad = output_pad;
        self.rebuild();
    }

    /// `rebuild` (`custom-entry.ts:48-69`): clear, run the renderer through
    /// the engine, adopt its component behind a spacer — or style the throw
    /// into the error box. The runner lock is dropped before the rebuilt
    /// container replaces the old one: dropping the previous proxy releases
    /// its JS handle, and the release re-locks the runner.
    fn rebuild(&mut self) {
        let mut container = BoxComponent::new(0, 0);
        self.has_content = false;

        let options = serde_json::json!({ "expanded": self.expanded }).to_string();
        let outcome = {
            let runner = self.runner.lock().expect("extension runner mutex poisoned");
            runner.render_entry(
                self.ext_index,
                &self.custom_type,
                &self.entry_json,
                &options,
            )
        };

        match outcome {
            Ok(pi_ext_js::EntryRenderOutcome::Component { handle, has_input }) => {
                let proxy = super::component_proxy::ComponentProxy::new(
                    Arc::clone(&self.runner),
                    handle,
                    has_input,
                );
                self.has_content = true;
                container.add_child(Box::new(pi_tui::components::spacer::Spacer::new(1)));
                container.add_child(Box::new(proxy));
            }
            // A renderer throw becomes the themed error box.
            Ok(pi_ext_js::EntryRenderOutcome::Failed(message)) => {
                self.build_error_box(&mut container, &message);
            }
            // A host-level engine failure surfaces the same way — upstream's
            // host cannot fail this call, so this arm is the port's addition.
            Err(error) => self.build_error_box(&mut container, &error.to_string()),
            Ok(pi_ext_js::EntryRenderOutcome::None) => {}
        }
        self.container = container;
    }

    /// The failure box (`custom-entry.ts:57-59`): `Box(outputPad, 1,
    /// customMessageBg)` wrapping the `[type] renderer failed: message`
    /// text in the error color — and the same `Spacer(1)` prefix the
    /// adopted component gets (`:62-68` — the spacer rides any content).
    fn build_error_box(&mut self, container: &mut BoxComponent, message: &str) {
        let mut box_component = BoxComponent::with_bg(self.output_pad, 1, |text| {
            theme::theme().bg(pi_tui::theme::ThemeBg::CustomMessageBg, text)
        });
        let text = theme::theme().fg(
            pi_tui::theme::ThemeColor::Error,
            &format!("[{}] renderer failed: {message}", self.custom_type),
        );
        box_component.add_child(Box::new(pi_tui::components::text::Text::new(text, 0, 0)));
        self.has_content = true;
        container.add_child(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        container.add_child(Box::new(box_component));
    }
}

impl Component for CustomEntryComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.container.render(width)
    }

    /// Upstream's `override invalidate()` re-runs `rebuild`, so a theme
    /// change re-renders the entry with the new mapping
    /// (`custom-entry.ts:43-46`).
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
