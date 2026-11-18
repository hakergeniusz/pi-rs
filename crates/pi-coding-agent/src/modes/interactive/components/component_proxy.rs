//! The adopted-JS-component proxy, shared by every component that embeds a
//! renderer's `Component` over the handle bridge — the Rust half of the
//! Component boundary (`tui.ts:124-155`). Landed with the entry renderers
//! (`custom-entry.ts`); the message renderers (`custom-message.ts`) reuse it.
//!
//! Only the numeric handle crosses the boundary: `render`/`handleInput`/
//! `invalidate` translate to the `__piComponent*` engine calls, and dropping
//! the proxy releases the handle so the JS registry does not leak.

use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};

use super::super::theme;

/// The adopted JS component, seen from Rust: only the handle crosses the
/// boundary. `render`/`handleInput`/`invalidate` translate to the
/// `__piComponent*` engine calls (`tui.ts:124-155`); dropping the proxy
/// releases the handle so the JS registry does not leak.
pub(crate) struct ComponentProxy {
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    handle: u32,
    has_input: bool,
}

impl ComponentProxy {
    /// Wraps a handle handed out by `render_entry`/`render_message`.
    pub(crate) fn new(
        runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
        handle: u32,
        has_input: bool,
    ) -> Self {
        Self {
            runner,
            handle,
            has_input,
        }
    }
}

impl Drop for ComponentProxy {
    fn drop(&mut self) {
        let _ = self
            .runner
            .lock()
            .expect("extension runner mutex poisoned")
            .component_release(self.handle);
    }
}

impl Component for ComponentProxy {
    fn render(&self, width: usize) -> Vec<String> {
        match self
            .runner
            .lock()
            .expect("extension runner mutex poisoned")
            .component_render(self.handle, width)
        {
            Ok(lines) => lines,
            // Upstream lets a component render throw take the frame down;
            // the port renders the failure in place instead.
            Err(error) => vec![theme::theme().fg(
                pi_tui::theme::ThemeColor::Error,
                &format!("component render failed: {error}"),
            )],
        }
    }

    /// The JS `handleInput(data) === true` decision is the consume bit;
    /// anything else (no handler, false, unknown handle) forwards the key.
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        let consumed = self
            .runner
            .lock()
            .expect("extension runner mutex poisoned")
            .component_handle_input(self.handle, data)
            .unwrap_or(false);
        Some(ComponentMutation { consume: consumed })
    }

    /// Whether the JS component implements `handleInput` (`tui.ts:90`).
    fn has_input_handler(&self) -> bool {
        self.has_input
    }

    fn invalidate(&mut self) {
        let _ = self
            .runner
            .lock()
            .expect("extension runner mutex poisoned")
            .component_invalidate(self.handle);
    }
}
