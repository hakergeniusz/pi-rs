//! The adopted-JS-component proxy, shared by every component that embeds a
//! renderer's `Component` over the handle bridge — the Rust half of the
//! Component boundary (`tui.ts:124-155`). Landed with the entry renderers
//! (`custom-entry.ts`); the message renderers (`custom-message.ts`) reuse it,
//! and the widget/footer/header mounts (E3 slice 7) build borrowed views
//! over handles the mode's registries own.
//!
//! Only the numeric handle crosses the boundary: `render`/`handleInput`/
//! `invalidate` translate to the `__piComponent*` engine calls, and an
//! OWNING proxy's drop releases the handle so the JS registry does not
//! leak. A borrowed view never releases — the registry side disposes.

use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};

use super::super::theme;

/// The adopted JS component, seen from Rust: only the handle crosses the
/// boundary. `render`/`handleInput`/`invalidate` translate to the
/// `__piComponent*` engine calls (`tui.ts:124-155`); an owning proxy's
/// drop releases the handle so the JS registry does not leak.
pub(crate) struct ComponentProxy {
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    handle: u32,
    has_input: bool,
    /// Whether dropping this view releases the handle. The widget/footer/
    /// header rebuilds wrap a handle the mode's registry still owns (E3
    /// slice 7) — their views must not release.
    owns_handle: bool,
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
            owns_handle: true,
        }
    }

    /// A view over a handle the mode's registry owns (E3 slice 7): the
    /// widget/footer/header mounts rebuild this per render; the registry
    /// side disposes the handle explicitly.
    pub(crate) fn borrowed(
        runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
        handle: u32,
        has_input: bool,
    ) -> Self {
        Self {
            runner,
            handle,
            has_input,
            owns_handle: false,
        }
    }

    /// The duck-typed `setExpanded` the custom header/footer swap calls
    /// (`interactive-mode.ts:2709-2711`/`:4749-4752`): a no-op when the
    /// component lacks the method — upstream checks `isExpandable` first.
    pub(crate) fn set_expanded(&self, expanded: bool) {
        let _ = self
            .runner
            .lock()
            .expect("extension runner mutex poisoned")
            .component_set_expanded(self.handle, expanded);
    }
}

impl Drop for ComponentProxy {
    fn drop(&mut self) {
        if !self.owns_handle {
            return;
        }
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

    // The proxy mounts in the widget/footer/header slots (E3 slice 7) —
    // the slot walks downcast to it, so the identity overrides are
    // load-bearing (a missing override makes the downcast silently no-op).
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}
