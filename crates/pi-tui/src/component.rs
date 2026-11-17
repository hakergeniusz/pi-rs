//! The component contract.
//!
//! Ground truth: `packages/tui/src/tui.ts:117-142` (interface) and
//! `:347-407` (`Container`), `:413-444` (`compositeTuiLine`).

use crate::ansi::SEGMENT_RESET;
use crate::ansi::{slice_by_column, slice_with_width, visible_width};
use crate::mouse::{MouseEvent, MouseEventResult, dispatch_mouse_event};

/// `LayoutBox.component` identity: the address of the trait-object fat pointer.
/// `dispatchMouseEvent` compares component references (`tui.ts:91`) and the
/// layout pass keys boxes on the same value.
pub use crate::mouse::component_id;

pub use crate::ansi::CURSOR_MARKER;

/// Owned components are `'static` boxes; this alias keeps the trait-object
/// lifetime explicit at every signature.
pub type DynComponent = dyn Component + 'static;

/// Anything that can be rendered into terminal lines.
///
/// `render` returns ANSI-styled lines; each line must fit the width it was
/// given, because the renderer refuses lines that are too wide rather than
/// corrupting the screen (`tui-main-screen.ts:517-544`).
pub trait Component: Send {
    /// Render to lines for a viewport `width` cells wide.
    fn render(&self, width: usize) -> Vec<String>;

    /// Optional keyboard handler for the focused component.
    fn handle_input(&mut self, _data: &str) -> Option<ComponentMutation> {
        None
    }

    /// When true the component also receives key release events (Kitty).
    /// `tui.ts:132-135`.
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Handle a normalized mouse event addressed to this component.
    /// `tui.ts:127-128`.
    ///
    /// `None` means "not mine"; returning a bare `{render: false}` also means
    /// not mine — see [`crate::mouse::dispatch_mouse_event`].
    fn handle_mouse(&mut self, _event: &MouseEvent) -> Option<MouseEventResult> {
        None
    }

    /// Whether this component implements `handleInput`. A container that
    /// forwards a child's `focus` keeps keyboard focus itself, because it also
    /// routes keys to that child. `tui.ts:90`.
    fn has_input_handler(&self) -> bool {
        false
    }

    /// Drop cached rendering state. `tui.ts:137-141`.
    fn invalidate(&mut self) {}

    /// Read-only companion to [`Component::as_any_mut`].
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }

    /// Escape hatch for the few host features that must reach a concrete
    /// component. Exactly two implement it, and each for the same reason:
    /// something in this crate must recover it from a `Box<dyn Component>`.
    ///
    /// - [`crate::components::scroll_view::ScrollView`], because the alt
    ///   screen's wheel routing and scrollbar need a `ScrollView` by identity
    ///   and `pi` gets that structurally (`tui-alt-screen.ts:988-996`,
    ///   `:1068-1078` hold live `ScrollView` references).
    /// - [`ImplicitDocument`], because `implicitScrollView` (`:268`) owns the
    ///   host's root children in a `Box` and every child operation has to
    ///   reach them.
    ///
    /// Nothing else in the tree downcasts, and nothing else needs to.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }

    /// Whether this component has a `handleMouse` of its own. pi's
    /// `dispatchMouseToLayout` skips a component that is a layout node whose
    /// only mouse behaviour is the inherited container band walk
    /// (`tui-alt-screen.ts:840`); `false` is that case.
    fn overrides_mouse_handler(&self) -> bool {
        true
    }

    /// The TUI marks the keyboard focus here. Components that show a hardware
    /// cursor implement it, usually by delegating to [`Focusable`].
    /// `tui.ts:174-187`.
    fn set_focused(&mut self, _focused: bool) {}

    /// Describe this component to the layout engine. `layout-node.ts:44-51`.
    /// `None` means "a leaf that renders its own lines".
    fn layout_node(&self) -> Option<crate::layout::LayoutNode<'_>> {
        None
    }

    /// Mutable child access for the layout engine's apply pass.
    /// Containers and stacks return their children; leaves return none.
    fn layout_children_mut(&mut self) -> Vec<&mut DynComponent> {
        Vec::new()
    }

    /// Mutable scroll state, for [`crate::layout::apply_scroll_updates`].
    fn scroll_state_mut(&mut self) -> Option<&mut crate::components::scroll_view::ScrollViewState> {
        None
    }
}

/// Result of handling input, returned to the host so it can react.
/// `tui.ts:144` (`TuiInputListenerResult`) collapsed to what 4a needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ComponentMutation {
    /// Input was consumed; do not forward it further.
    pub consume: bool,
}

/// A component that can show a hardware cursor when focused.
/// `tui.ts:174-187`.
pub trait Focusable {
    /// Show or hide this component's focused indicator.
    fn set_focused(&mut self, focused: bool);
}

/// Stacks children vertically, concatenating their lines. `tui.ts:347-407`.
#[derive(Default)]
pub struct Container {
    children: Vec<Box<dyn Component>>,
    /// `(render width, child heights)` from the last render, so the mouse
    /// pass does not have to re-render. `tui.ts:356-365`.
    mouse_layout: std::cell::RefCell<Option<(usize, Vec<usize>)>>,
}

impl std::fmt::Debug for Container {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Container")
            .field("children", &self.children.len())
            .field("mouse_layout", &self.mouse_layout.borrow())
            .finish()
    }
}

impl Container {
    /// An empty container.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt an existing child list. `getRenderRoots`, `tui-alt-screen.ts:332`:
    /// with no explicit layout root the render roots are the host's own
    /// children, so the alt screen lays them out through exactly this shape.
    pub fn from_children(children: Vec<Box<dyn Component>>) -> Self {
        Self {
            children,
            mouse_layout: std::cell::RefCell::new(None),
        }
    }

    /// Give the child list back, in order.
    pub fn into_children(self) -> Vec<Box<dyn Component>> {
        self.children
    }

    /// Take the child list out through a mutable borrow, in order. The chat
    /// viewport's disassemble reaches the document container by identity, so
    /// ownership has to move through `&mut self`.
    pub fn take_children(&mut self) -> Vec<Box<dyn Component>> {
        std::mem::take(&mut self.children)
    }

    /// Append a child; it renders below the children already present.
    pub fn add_child(&mut self, component: Box<dyn Component>) {
        self.children.push(component);
    }

    /// Remove and return the child at `index`; the children after it shift
    /// down one slot.
    pub fn remove_child(&mut self, index: usize) -> Box<dyn Component> {
        self.children.remove(index)
    }

    /// Remove every child.
    pub fn clear(&mut self) {
        self.children.clear();
    }

    /// How many children are mounted.
    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// Whether there are no children.
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// The child at `index`, if any.
    pub fn get(&self, index: usize) -> Option<&DynComponent> {
        self.children.get(index).map(AsRef::as_ref)
    }

    /// The child at `index`, mutably.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut DynComponent> {
        self.children.get_mut(index).map(AsMut::as_mut)
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let mut heights = Vec::with_capacity(self.children.len());
        for child in &self.children {
            let child_lines = child.render(width);
            heights.push(child_lines.len());
            lines.extend(child_lines);
        }
        *self.mouse_layout.borrow_mut() = Some((width, heights));
        lines
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        for child in &mut self.children {
            if let Some(mutation) = child.handle_input(data) && mutation.consume {
                return Some(mutation);
            }
        }
        None
    }

    fn invalidate(&mut self) {
        *self.mouse_layout.borrow_mut() = None;
        for child in &mut self.children {
            child.invalidate();
        }
    }

    fn layout_children_mut(&mut self) -> Vec<&mut DynComponent> {
        self.children.iter_mut().map(AsMut::as_mut).collect()
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    /// Hosts reach into the tree with typed children (e.g. the chat
    /// container), so containers opt into downcasting.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    /// `Container.handleMouse`, `tui.ts:372-392`. Walks the stacked bands and
    /// forwards to the child under the pointer.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        if event.y < 0 || event.y >= event.height as i64 {
            return None;
        }
        let cached = self.mouse_layout.borrow().clone();
        let host_id = component_id(&*self);
        dispatch_mouse_band(
            host_id,
            self.has_input_handler(),
            &mut self.children,
            cached.as_ref(),
            event,
            event.x,
            event.y,
            event.width,
        )
    }
}

/// Walk stacked child bands and forward the event to the one under the
/// pointer. Shared by `Container` (`tui.ts:372-392`) and `Box`
/// (`components/box.ts:75-102`), which differ only in how they derive
/// `x`/`y`/`width` from their padding.
///
/// The forwarded result keeps the child's own dispatch target; when the child
/// asked for focus and this host also routes keys, the host claims focus so a
/// child detached later (a closed settings submenu) cannot strand it.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_mouse_band(
    host_id: crate::mouse::ComponentId,
    host_handles_input: bool,
    children: &mut [Box<dyn Component>],
    cached: Option<&(usize, Vec<usize>)>,
    event: &MouseEvent,
    x: i64,
    y: i64,
    width: usize,
) -> Option<MouseEventResult> {
    let heights: Vec<usize> = match cached {
        Some((cached_width, heights)) if *cached_width == width => heights.clone(),
        _ => children
            .iter()
            .map(|child| child.render(width).len())
            .collect(),
    };
    let mut child_y = 0i64;
    for (child, height) in children.iter_mut().zip(heights) {
        if y >= child_y && y < child_y + height as i64 {
            let child_event = MouseEvent {
                x,
                y: y - child_y,
                width,
                height,
                ..*event
            };
            let result = dispatch_mouse_event(child.as_mut(), &child_event)?;
            let focus_target = if result.focus.unwrap_or(false) && host_handles_input {
                Some(host_id)
            } else {
                result.focus_target
            };
            return Some(MouseEventResult {
                handled: Some(true),
                capture: result.capture,
                focus: result.focus,
                render: result.render,
                target: Some(result.target),
                focus_target,
            });
        }
        child_y += height as i64;
    }
    None
}

/// `implicitDocument`, `tui-alt-screen.ts:261-267`: the object literal pi
/// builds around the alt screen's own `Container` so the implicit scroll view
/// has a real component to own.
///
/// It is a `Container` under another name — `render` is `super.render`,
/// `handleMouse` is `super.handleMouse`, `invalidate` walks the children —
/// with no `handleInput` of its own, so pi's `focusTarget: this` branch in
/// `Container.handleMouse` does not fire for it.
pub(crate) struct ImplicitDocument {
    children: Vec<Box<dyn Component>>,
}

impl ImplicitDocument {
    pub(crate) fn new(children: Vec<Box<dyn Component>>) -> Self {
        Self { children }
    }

    pub(crate) fn children(&self) -> &[Box<dyn Component>] {
        &self.children
    }

    /// Empty the document, handing its children back — [`Tui::exit_alt_screen`]
    /// returns whatever the host left mounted when it leaves the alt screen.
    pub(crate) fn take_children(&mut self) -> Vec<Box<dyn Component>> {
        std::mem::take(&mut self.children)
    }

    pub(crate) fn children_mut(&mut self) -> &mut Vec<Box<dyn Component>> {
        &mut self.children
    }
}

impl std::fmt::Debug for ImplicitDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImplicitDocument")
            .field("children", &self.children.len())
            .finish()
    }
}

impl Component for ImplicitDocument {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        let host_id = component_id(&*self);
        dispatch_mouse_band(
            host_id,
            false,
            &mut self.children,
            None,
            event,
            event.x,
            event.y,
            event.width,
        )
    }
}

/// The `implicitDocument` behind a component, if that is what it is.
pub(crate) fn implicit_document_of(child: &dyn Component) -> Option<&ImplicitDocument> {
    child
        .as_any()
        .and_then(|any| any.downcast_ref::<ImplicitDocument>())
}

pub(crate) fn implicit_document_mut(
    child: &mut dyn Component,
) -> Option<&mut Vec<Box<dyn Component>>> {
    child
        .as_any_mut()?
        .downcast_mut::<ImplicitDocument>()
        .map(ImplicitDocument::children_mut)
}

/// Composite overlay content into a line at a fixed column. `tui.ts:415-444`.
pub fn composite_tui_line(
    base_line: &str,
    overlay_line: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    let after_start = start_col + overlay_width;
    let base = crate::ansi::extract_segments(
        base_line,
        start_col,
        after_start,
        total_width.saturating_sub(after_start),
        true,
    );
    let overlay = slice_with_width(overlay_line, 0, overlay_width, true);
    let before_pad = start_col.saturating_sub(base.before_width);
    let overlay_pad = overlay_width.saturating_sub(overlay.width);
    let actual_before_width = start_col.max(base.before_width);
    let actual_overlay_width = overlay_width.max(overlay.width);
    let after_target = total_width.saturating_sub(actual_before_width + actual_overlay_width);
    let after_pad = after_target.saturating_sub(base.after_width);
    let result = format!(
        "{}{}{}{}{}{}{}",
        base.before,
        " ".repeat(before_pad),
        SEGMENT_RESET,
        overlay.text,
        " ".repeat(overlay_pad),
        SEGMENT_RESET,
        base.after,
    );
    let _ = after_pad;
    let padded = format!("{}{}", result, " ".repeat(after_pad));
    if visible_width(&padded) <= total_width {
        padded
    } else {
        slice_by_column(&padded, 0, total_width, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lines(&'static [&'static str]);

    impl Component for Lines {
        fn render(&self, _width: usize) -> Vec<String> {
            self.0.iter().map(ToString::to_string).collect()
        }
    }

    struct Recorder {
        seen: Vec<String>,
    }

    impl Component for Recorder {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![]
        }

        fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
            self.seen.push(data.to_string());
            Some(ComponentMutation { consume: true })
        }
    }

    #[test]
    fn container_concatenates_child_lines() {
        let mut container = Container::new();
        container.add_child(Box::new(Lines(&["a", "b"])));
        container.add_child(Box::new(Lines(&["c"])));
        assert_eq!(container.render(10), vec!["a", "b", "c"]);
    }

    #[test]
    fn container_forwards_input_until_consumed() {
        let mut container = Container::new();
        container.add_child(Box::new(Recorder { seen: Vec::new() }));
        assert!(container.handle_input("x").unwrap().consume);
    }

    #[test]
    fn composite_pads_the_overlay_into_place() {
        let line = composite_tui_line("aaaa", "bb", 2, 2, 6);
        assert_eq!(visible_width(&line), 6);
        assert_eq!(crate::ansi::strip_terminal_sequences(&line), "aabb  ");
    }

    #[test]
    fn composite_extends_short_base_lines() {
        // `extractSegments` treats a line narrower than `start_col` as all
        // "before" (`utils.ts`), so the base content stays at column 0 and
        // the overlay lands on top of it.
        let line = composite_tui_line("aa", "bb", 2, 2, 8);
        assert_eq!(crate::ansi::strip_terminal_sequences(&line), "aabb    ");
    }

    #[test]
    fn composite_never_exceeds_the_total_width() {
        let line = composite_tui_line("aaaaaaa", "bbb", 1, 3, 5);
        assert_eq!(visible_width(&line), 5);
    }
}
