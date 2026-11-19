//! `ExtensionCustomComponent` — the port-side wrapper for a `ctx.ui.custom`
//! component (E3 slice 6b).
//!
//! Ground truth: `showExtensionCustom` (`interactive-mode.ts:3074-3163`)
//! mounts the factory's component directly; the port's engine is
//! out-of-process, so the wrapper holds only the adopted handle and every
//! render/input call crosses through the runner — the same seam the tool
//! renderers use (`tool_execution.rs`). `__piComponentAdopt` requires the
//! JS component to implement `render(width)` and `invalidate()`; its
//! optional `handleInput(data)` answers with `true` when it consumed the
//! key. The TUI re-renders after every input by itself
//! (`request_immediate_render`), so a component that changed its state in
//! `handleInput` shows without any extra crossing; its `tui.requestRender()`
//! adapter crossing lands on the next pump either way.

use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};

/// One mounted custom component. `handle` is the engine-side registry id;
/// the mode disposes it through [`pi_ext_js::ExtensionRunner::component_dispose`]
/// on close (upstream's `disposeComponent`).
pub struct ExtensionCustomComponent {
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    handle: u32,
}

impl ExtensionCustomComponent {
    pub fn new(runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>, handle: u32) -> Self {
        Self { runner, handle }
    }

    /// The engine-side handle, for the close path's dispose.
    pub fn handle(&self) -> u32 {
        self.handle
    }
}

impl Component for ExtensionCustomComponent {
    /// The component's `render(width)` lines. A crossing error renders as
    /// an error line — upstream's render throws surface through the TUI's
    /// error handling; the port keeps the dialog alive and shows why.
    fn render(&self, width: usize) -> Vec<String> {
        match self
            .runner
            .lock()
            .expect("extension runner mutex poisoned")
            .component_render(self.handle, width)
        {
            Ok(lines) => lines,
            Err(err) => vec![format!("Error: custom component render failed: {err}")],
        }
    }

    /// The component's `handleInput(data)` — its `true` answer consumes
    /// the key (the glue normalizes it to `{consume: === true}`).
    /// `done()` may fire inside the crossing; the pump's next
    /// [`service_ui_requests`](pi_ext_js::ExtensionRunner::service_ui_requests)
    /// picks the close up.
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        let consumed = self
            .runner
            .lock()
            .expect("extension runner mutex poisoned")
            .component_handle_input(self.handle, data)
            .unwrap_or(false);
        Some(ComponentMutation { consume: consumed })
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ext_js::{EntryRenderOutcome, HostCore};

    /// A component whose render lines and consumption answers cross
    /// through the runner — the wrapper mirrors the engine's registry.
    /// Adoption rides the entry-renderer path (`render_entry`), the same
    /// `__piComponentAdopt` glue the factory's component takes.
    #[test]
    fn render_and_input_cross_through_the_runner() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", () => ({
    render: (width) => ["CUSTOM LINE " + width],
    invalidate: () => {},
    handleInput: (data) => data === "x",
  }));
}
"#;
        const ENTRY: &str = r#"{"type":"custom","id":"e1","timestamp":"2026-01-01T00:00:00Z","customType":"note","data":{"k":1}}"#;
        let mut runner = pi_ext_js::ExtensionRunner::new(HostCore::default()).expect("runner");
        runner.load_extension("<test-0>", Some(EXT)).expect("load");
        runner.bind().expect("bind");
        let outcome = runner
            .render_entry(0, "note", ENTRY, "{}")
            .expect("render entry");
        let EntryRenderOutcome::Component { handle, has_input } = outcome else {
            panic!("expected an adopted component, got {outcome:?}");
        };
        assert!(has_input);
        let mut wrapper = ExtensionCustomComponent::new(Arc::new(Mutex::new(runner)), handle);
        assert_eq!(wrapper.render(42), vec!["CUSTOM LINE 42".to_string()]);
        assert!(wrapper.handle_input("x").expect("mutation").consume);
        assert!(!wrapper.handle_input("y").expect("mutation").consume);
    }
}
