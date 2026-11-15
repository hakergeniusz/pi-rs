//! The differential renderer, overlays and focus.
//!
//! Ground truth: `packages/tui/src/tui.ts` (`TuiBase`, `Container`,
//! `compositeTuiLine`) and `packages/tui/src/tui-main-screen.ts`
//! (`TuiMainScreen.doRender`).
//!
//! Deliberate deviations, all of them mechanical:
//!
//! * No timers. pi schedules a frame with `setTimeout`; here
//!   [`Tui::request_render`] sets a flag and the host calls
//!   [`Tui::render_now`]. The terminal-colour query works the same way: the
//!   caller passes the host clock to [`Tui::service_terminal_queries`].
//! * Kitty image lines are recognised ([`crate::terminal_image::is_image_line`])
//!   but no component emits them yet; that lands with the alt-screen renderer.
//! * The terminal-colour and cell-size protocols are exposed as query methods
//!   rather than pushed by the host: [`Tui::query_terminal_colors`],
//!   [`Tui::query_cell_size`]. Replies are consumed by
//!   [`Tui::consume_terminal_response`].
//!
//! pi's colour-scheme listeners live in a `Set`
//! (`onTerminalColorSchemeChange`, `tui.ts:943-948`), which dedupes an
//! identical listener reference and may be edited while it is walked. Both
//! halves are ported over an [`Arc`]-shared closure and an id: two clones of
//! one [`ColorSchemeListener`] are the same pi reference, so registering one
//! function twice keeps one entry and hands back the id that already names it
//! ([`Tui::add_color_scheme_listener`]), and the dispatch re-checks the id
//! under its cursor so a listener removed mid-walk is honoured.

use std::io;
use std::sync::{Arc, Mutex};

use crate::ansi::{
    SEGMENT_RESET, normalize_terminal_output, strip_terminal_sequences, visible_width,
};
use crate::component::Component;
use crate::frame::CellGrid;
use crate::keys::{is_key_release, matches_key};
use crate::mouse::{ComponentId, component_id};
use crate::terminal::{Terminal, TerminalSize};
use crate::terminal_colors::{
    OscColorTarget, RgbColor, TERMINAL_COLOR_REPLY_COUNT, TERMINAL_PALETTE_SIZE,
    TerminalColorScheme, is_cell_size_response, is_device_attributes_response,
    parse_cell_size_response, parse_osc_color_response, parse_terminal_color_scheme_report,
    terminal_color_query,
};

/// Begin synchronized output. `tui-main-screen.ts:280`.
const BEGIN_SYNC: &str = "\x1b[?2026h";
/// End synchronized output. `tui-main-screen.ts:302`.
const END_SYNC: &str = "\x1b[?2026l";

/// Overlay anchor. `tui.ts:203-212`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAnchor {
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

/// Margins from the terminal edges. `tui.ts:215-222`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlayMargin {
    pub top: Option<i64>,
    pub right: Option<i64>,
    pub bottom: Option<i64>,
    pub left: Option<i64>,
}

/// Absolute cells or a percentage string like `"50%"`. `tui.ts:225`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeValue {
    Cells(i64),
    Percent(f64),
}

impl SizeValue {
    /// `tui.ts:228-237`.
    fn resolve(&self, reference: i64) -> i64 {
        match self {
            SizeValue::Cells(value) => *value,
            SizeValue::Percent(percent) => ((reference as f64) * percent / 100.0).floor() as i64,
        }
    }
}

/// Overlay placement and visibility. `tui.ts:243-279`.
#[derive(Clone)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub min_width: Option<i64>,
    pub max_height: Option<SizeValue>,
    pub anchor: OverlayAnchor,
    pub offset_x: i64,
    pub offset_y: i64,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    pub margin: OverlayMargin,
    /// Predicate evaluated every frame with the terminal size.
    pub visible: Option<std::sync::Arc<dyn Fn(u16, u16) -> bool + Send + Sync>>,
    pub non_capturing: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            width: None,
            min_width: None,
            max_height: None,
            anchor: OverlayAnchor::Center,
            offset_x: 0,
            offset_y: 0,
            row: None,
            col: None,
            margin: OverlayMargin::default(),
            visible: None,
            non_capturing: false,
        }
    }
}

impl std::fmt::Debug for OverlayOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayOptions")
            .field("width", &self.width)
            .field("min_width", &self.min_width)
            .field("max_height", &self.max_height)
            .field("anchor", &self.anchor)
            .field("offset_x", &self.offset_x)
            .field("offset_y", &self.offset_y)
            .field("row", &self.row)
            .field("col", &self.col)
            .field("non_capturing", &self.non_capturing)
            .finish_non_exhaustive()
    }
}

/// Where the overlay landed last frame. `tui.ts:288-293`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayBounds {
    pub row: i64,
    pub col: i64,
    pub width: usize,
    pub height: usize,
}

/// Identifies an overlay for the lifetime of one registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OverlayId {
    index: usize,
    generation: u64,
}

/// Identifies a colour-scheme listener for the lifetime of one registration.
/// Like [`OverlayId`], ids carry a generation, so dropping one listener never
/// shifts the ids of the ones after it.
#[derive(Debug, Clone, Copy, Eq)]
pub struct ColorSchemeListenerId {
    generation: u64,
}

impl PartialEq for ColorSchemeListenerId {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation
    }
}

impl std::hash::Hash for ColorSchemeListenerId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.generation.hash(state);
    }
}

/// A colour-scheme listener. [`Arc`] because the registration and the dedupe
/// key both need the closure's identity: two clones of one `Arc` are one pi
/// `Set` entry, exactly as two references to one JS function are.
pub type ColorSchemeListener = Arc<dyn Fn(TerminalColorScheme) + Send + Sync>;

/// A registered colour-scheme listener and the generation that names it.
///
/// The closure is an [`Option`] because the dispatch moves it out for the call
/// and puts it back afterwards: the lock must not be held while a listener
/// runs, or a listener that unsubscribes itself would deadlock.
struct ColorSchemeListenerEntry {
    generation: u64,
    listener: Option<ColorSchemeListener>,
}

/// The generation and the closure the dispatch moved out of one entry: what it
/// calls, and what it puts back under the id it re-checks.
type TakenColorSchemeListener = (u64, ColorSchemeListener);

/// The address of a listener's shared closure. pi compares function references
/// (`Set.add`, `tui.ts:944`); the `Arc` allocation address is that reference
/// here. A listener currently moved out for its call keeps the same entry slot
/// with the closure absent, so its key reads as `None` and no second
/// registration can claim it mid-call.
fn color_scheme_listener_key(listener: Option<&ColorSchemeListener>) -> Option<usize> {
    listener.map(|shared| Arc::as_ptr(shared).cast::<()>() as usize)
}

/// pi's live listener `Set` (`tui.ts:943-948`), shared between the TUI and the
/// listeners themselves.
///
/// pi deletes from the same `Set` it iterates (`tui.ts:1174-1176`), so a
/// listener removed while the dispatch is running is gone for the rest of that
/// report. The list is behind an [`Arc`] whose lock is held only while reading
/// or editing it — never while a listener runs — so
/// [`ColorSchemeListenerRegistry::remove`] is reachable from inside a
/// listener, which is what pi's unsubscribe closure gives it for free.
///
/// [`Tui::color_scheme_listener_registry`] mints one; every clone addresses the
/// same list.
#[derive(Clone, Default)]
pub struct ColorSchemeListenerRegistry {
    listeners: Arc<Mutex<Vec<ColorSchemeListenerEntry>>>,
}

impl ColorSchemeListenerRegistry {
    fn set(&self) -> std::sync::MutexGuard<'_, Vec<ColorSchemeListenerEntry>> {
        // The lock is never held across a listener call, so the only way to
        // see it poisoned is a panic in `add`/`remove`, which touch no user
        // code.
        self.listeners.lock().expect("color scheme listeners")
    }

    /// The generation already naming `listener`, if any. pi's `Set.add`
    /// (`tui.ts:944`) returns the entry it found instead of storing a second
    /// one; this is the read half of that, keyed on the shared closure's
    /// address.
    fn generation_of(&self, listener: &ColorSchemeListener) -> Option<u64> {
        let key = Arc::as_ptr(listener).cast::<()>() as usize;
        self.set()
            .iter()
            .find(|entry| color_scheme_listener_key(entry.listener.as_ref()) == Some(key))
            .map(|entry| entry.generation)
    }

    /// pi's unsubscribe closure (`tui.ts:945-947`): delete exactly the
    /// listener that made it and leave the rest in registration order. An id
    /// that already fired removes nothing, like `Set.delete`.
    pub fn remove(&self, id: ColorSchemeListenerId) {
        self.set().retain(|entry| entry.generation != id.generation);
    }

    /// How many listeners are registered.
    pub fn len(&self) -> usize {
        self.set().len()
    }

    /// Whether no listener is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for ColorSchemeListenerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = self.set();
        f.debug_struct("ColorSchemeListenerRegistry")
            .field("listeners", &set.len())
            .finish()
    }
}

struct OverlayEntry {
    component: Box<dyn Component>,
    options: OverlayOptions,
    pre_focus: Option<FocusTarget>,
    /// The component that held focus before this overlay opened, by identity.
    /// The blocked/eligible walk needs the component, not the handle
    /// (`tui.ts:684-695`).
    pre_focus_id: Option<ComponentId>,
    hidden: bool,
    focus_order: u64,
    bounds: Option<OverlayBounds>,
    generation: u64,
}

/// What a blocked overlay should resume to. `tui.ts:331`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayFocusResume {
    /// Give focus back to the overlay itself.
    RestoreOverlay,
    /// Give focus to an explicit target, or to nothing.
    FocusTarget(Option<FocusTarget>),
}

/// The nested-overlay focus-restore state machine. `tui.ts:329-337`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayFocusRestoreState {
    /// Nothing pending.
    Inactive,
    /// An overlay holds focus; a child focus change away from it would block.
    Eligible { overlay: OverlayId },
    /// A focus change was blocked so the overlay can take it back when the
    /// child that stole it goes away.
    Blocked {
        overlay: OverlayId,
        blocked_by: ComponentId,
        resume: OverlayFocusResume,
    },
}

/// What `set_focus` does to a pending blocked state. `tui.ts:337`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayFocusRestorePolicy {
    Clear,
    Preserve,
}

/// Who has the keyboard. `tui.ts:583-586`.
///
/// The `Id` arm is the port's fullscreen adaptation: upstream focuses
/// component *objects*, which stay valid when the chat viewport mounts them
/// inside its dock. Rust ownership moves those components under the viewport
/// root, out of the flat root-children list, so a nested focus target (the
/// editor, or a selector dialog swapped into the editor's dock slot) can only
/// be named by identity. [`Tui::component_mut`] deep-walks the tree for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    Root(usize),
    Overlay(OverlayId),
    /// A mounted component by identity, at any depth.
    Id(ComponentId),
}

/// Cursor position taken from the render output. `tui.ts:1442`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    pub row: usize,
    pub col: usize,
}

/// What the terminal reported for its current theme. `terminal-colors.ts:9-17`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalColors {
    /// Default foreground (OSC 10).
    pub foreground: Option<RgbColor>,
    /// Default background (OSC 11).
    pub background: Option<RgbColor>,
    /// ANSI colours 0-15. Only set when all sixteen arrived.
    pub palette: Option<[RgbColor; TERMINAL_PALETTE_SIZE]>,
}

/// One in-flight colour query. `tui.ts:1475-1487`.
struct PendingTerminalColorQuery {
    palette: [Option<RgbColor>; TERMINAL_PALETTE_SIZE],
    foreground: Option<RgbColor>,
    background: Option<RgbColor>,
    /// Indices into [`Tui::pending_color_queries`] that already answered.
    replied: Vec<bool>,
    replied_count: usize,
    /// Host deadline in milliseconds. This crate owns no timer, so the host
    /// services it in [`Tui::service_terminal_queries`].
    deadline_ms: u64,
    /// pi's `deliver` (`tui.ts:154-157`). One slot with three states: the
    /// promise while the query is live, the query's own `onLateReply` once
    /// the deadline swaps it, and unset once the query has settled.
    delivery: TerminalColorDelivery,
    /// pi's `onLateReply`, taken over when the deadline fires. It lives on the
    /// query, so a late reply reaches the query that asked for it and not
    /// whichever one happens to be first.
    late_sink: Option<Box<dyn FnMut(TerminalColors) + Send>>,
}

/// pi's `PendingTerminalColorQuery.deliver`. `tui.ts:154-157`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TerminalColorDelivery {
    /// Still the promise's `resolve`: the result joins
    /// [`Tui::terminal_color_results`].
    Resolve,
    /// The deadline fired, so `deliver` points at this query's `onLateReply`.
    Late,
    /// `deliver` was unset: a later completion delivers nothing, which is pi's
    /// `deliver?.(...)` no-op.
    Done,
}

impl PendingTerminalColorQuery {
    /// `tui.ts:1132`: pi keeps collecting replies only while `deliver` is set.
    fn delivers(&self) -> bool {
        self.delivery != TerminalColorDelivery::Done
    }
}
/// The TUI. `tui.ts:446-...` (TUI interface) and `tui-main-screen.ts:124`.
// The booleans mirror upstream TS state flags; the port keeps them one-to-one.
#[allow(clippy::struct_excessive_bools)]
pub struct Tui {
    terminal: Box<dyn Terminal>,
    children: Vec<Box<dyn Component>>,
    overlays: Vec<OverlayEntry>,
    focused: Option<FocusTarget>,
    focused_component: Option<usize>,
    focus_order_counter: u64,
    // Render state. `tui-main-screen.ts:126-133`.
    previous_lines: Vec<String>,
    previous_width: i64,
    previous_height: i64,
    cursor_row: i64,
    hardware_cursor_row: i64,
    max_lines_rendered: usize,
    previous_viewport_top: i64,
    full_redraw_count: usize,
    render_requested: bool,
    /// A pending request that bypasses the 16 ms floor, set by
    /// [`Tui::request_immediate_render`]. `tui.ts:517`.
    ///
    /// pi cancels the throttled timer and renders on the next tick
    /// (`requestImmediateRender`, `tui.ts:1001-1017`); here the host polls
    /// [`Tui::render_due_at`], which reports due at once while this is set.
    immediate_render: bool,
    /// When the last frame went out. `tui.ts:506` `lastRenderAt`; `None` until
    /// the first one, which is what makes the first request due at once.
    last_render_at: Option<std::time::Instant>,
    stopped: bool,
    clear_on_shrink: bool,
    show_hardware_cursor: bool,
    last_frame: Option<String>,
    on_render_error: Option<Box<dyn Fn(String) + Send + Sync>>,
    /// Identity of the focused component, overlay or root. `tui.ts:597`.
    focused_id: Option<ComponentId>,
    overlay_focus_restore: OverlayFocusRestoreState,
    pending_color_queries: Vec<PendingTerminalColorQuery>,
    /// Colours from queries the DA1 fence (or the deadline) completed.
    terminal_color_results: Vec<TerminalColors>,
    /// Colour-scheme reports received since the last drain. `tui.ts:1170-1176`.
    pending_color_scheme_reports: Vec<TerminalColorScheme>,
    /// pi's live colour-scheme listener `Set`. `tui.ts:943-948`.
    color_scheme_listeners: ColorSchemeListenerRegistry,
    /// Generation source for [`ColorSchemeListenerId`]; never reused, so an
    /// id from a removed listener can never name the next one.
    color_scheme_listener_counter: u64,
    /// Whether the terminal was asked to report its colour scheme at all.
    /// `tui.ts:518`; off until [`Tui::set_color_scheme_notifications`] turns
    /// it on, so `start` writes no mode for a host that never listens.
    color_scheme_notifications: bool,
    /// The alt-screen mode's state. `None` until a host enters it, so the
    /// main-screen path pays nothing.
    pub alt_screen: Option<crate::tui_alt_screen::AltScreenState>,
    /// Which root child the alt screen mounted, if any.
    pub(crate) alt_root: Option<usize>,
    /// The primary scroll view of the last alt-screen layout pass.
    alt_primary_scroll_view: Option<ComponentId>,
}

/// Options for starting. `tui-alt-screen.ts:270-278`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuiStartOptions {
    /// Track the mouse. `mouse ?? true`.
    pub mouse: bool,
}

impl Default for TuiStartOptions {
    fn default() -> Self {
        Self { mouse: true }
    }
}

/// Options for stopping. `tui.ts:448-451`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TuiStopOptions {
    /// Leave the rendered output in place for another TUI.
    pub preserve_screen: bool,
}

/// What the next frame does. The three arms are the three exits of
/// `tui-main-screen.ts:330-360` before the differential path.
/// `tui-main-screen.ts:338/345/357`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redraw {
    /// Nothing was on screen and nothing changed: draw without clearing.
    First,
    /// Repaint every line, clearing the screen (and scrollback) first.
    Full,
    /// Repaint only the lines that changed.
    Incremental,
}

/// Everything [`redraw_for`] reads. Struct-wrapped so the decision stays a pure
/// function a host (and a test) can drive with injected values.
// One flag per upstream input; the resize rule reads them directly.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy)]
pub struct RedrawInputs {
    /// The geometry of the last frame, or `None` when nothing has been drawn.
    pub previous_size: Option<TerminalSize>,
    /// The geometry the terminal reports now.
    pub size: TerminalSize,
    /// Whether the last frame wrote any lines.
    pub has_previous_lines: bool,
    /// Lines the next frame will write.
    pub new_line_count: usize,
    /// Lines the tallest frame so far needed. pi keeps this so a frame that
    /// shrank knows it left rows behind (`tui-main-screen.ts:357`).
    pub max_lines_rendered: usize,
    /// `Tui.setClearOnShrink` (`tui-main-screen.ts:359`).
    pub clear_on_shrink: bool,
    /// Overlays need the padding, so a shrink must not clear under them
    /// (`tui-main-screen.ts:356`).
    pub has_overlays: bool,
    /// Termux changes its reported height when the software keyboard toggles
    /// (`tui-main-screen.ts:351`).
    pub termux: bool,
}

/// The resize rule, as a decision. `tui-main-screen.ts:330-360`.
///
/// Width changes always repaint because wrapping changes; height changes do
/// too, except on Termux; and content that shrank below the working area
/// repaints only when an overlay is *not* up, because an overlay is positioned
/// against the rows a clear would erase.
pub fn redraw_for(inputs: RedrawInputs) -> Redraw {
    let previous = inputs.previous_size;
    let width_changed = previous.is_some_and(|previous| previous.columns != inputs.size.columns);
    let height_changed = previous.is_some_and(|previous| previous.rows != inputs.size.rows);
    if !inputs.has_previous_lines && !width_changed && !height_changed {
        return Redraw::First;
    }
    if width_changed || (height_changed && !inputs.termux) {
        return Redraw::Full;
    }
    if inputs.clear_on_shrink
        && inputs.new_line_count < inputs.max_lines_rendered
        && !inputs.has_overlays
    {
        return Redraw::Full;
    }
    Redraw::Incremental
}

/// The renderer's previous geometry, or `None` before the first frame.
///
/// pi guards both comparisons with `previousWidth !== 0` / `previousHeight !== 0`
/// (`tui-main-screen.ts:338-339`), and `reset_render_state` writes `-1`
/// (`tui-main-screen.ts:158-166`); either way no frame has been drawn, so both
/// mean the same thing. The two fields are only ever written together.
fn previous_size(width: i64, height: i64) -> Option<TerminalSize> {
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(TerminalSize {
        columns: width as u16,
        rows: height as u16,
    })
}

impl Tui {
    /// `tui.ts:520-...`.
    pub fn new(terminal: Box<dyn Terminal>) -> Self {
        Self {
            terminal,
            children: Vec::new(),
            overlays: Vec::new(),
            focused: None,
            focused_component: None,
            focus_order_counter: 0,
            previous_lines: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            cursor_row: 0,
            hardware_cursor_row: 0,
            max_lines_rendered: 0,
            previous_viewport_top: 0,
            full_redraw_count: 0,
            render_requested: false,
            immediate_render: false,
            color_scheme_notifications: false,
            last_render_at: None,
            stopped: true,
            clear_on_shrink: false,
            show_hardware_cursor: false,
            last_frame: None,
            on_render_error: None,
            focused_id: None,
            overlay_focus_restore: OverlayFocusRestoreState::Inactive,
            pending_color_queries: Vec::new(),
            terminal_color_results: Vec::new(),
            pending_color_scheme_reports: Vec::new(),
            color_scheme_listeners: ColorSchemeListenerRegistry::default(),
            color_scheme_listener_counter: 0,
            alt_screen: None,
            alt_root: None,
            alt_primary_scroll_view: None,
        }
    }

    /// Called when a rendered line exceeds the terminal width. pi throws
    /// (`tui-main-screen.ts:535-543`); tests and hosts prefer a callback.
    pub fn set_render_error_handler(&mut self, handler: Box<dyn Fn(String) + Send + Sync>) {
        self.on_render_error = Some(handler);
    }

    /// `tui.ts:1052-1057` (`addChild`).
    pub fn add_child(&mut self, component: Box<dyn Component>) -> usize {
        self.root_children_mut().push(component);
        self.request_render();
        self.root_children().len() - 1
    }

    /// `tui.ts:1059-1064` (`removeChild`).
    pub fn remove_child(&mut self, index: usize) -> Box<dyn Component> {
        if self.focused == Some(FocusTarget::Root(index)) {
            self.set_focus(None);
        }
        self.request_render();
        self.root_children_mut().remove(index)
    }

    /// Swap the child at `index`, returning the old one. Hosts use this to
    /// mount a selector dialog in place of the editor and put it back
    /// (`showSelector`, `interactive-mode.ts:5072-5096`, swaps inside the
    /// editor container).
    pub fn replace_child(
        &mut self,
        index: usize,
        component: Box<dyn Component>,
    ) -> Box<dyn Component> {
        if self.focused == Some(FocusTarget::Root(index)) {
            self.set_focus(None);
        }
        self.request_render();
        self.root_children_mut().insert(index, component);
        self.root_children_mut().remove(index + 1)
    }

    /// Drop every root child, as `Container.clear` does. `tui.ts:1066-1069`.
    pub fn clear_children(&mut self) {
        self.set_focus(None);
        self.root_children_mut().clear();
        self.request_render();
    }

    pub fn children_len(&self) -> usize {
        self.root_children().len()
    }

    pub fn child(&self, index: usize) -> Option<&crate::component::DynComponent> {
        self.root_children().get(index).map(AsRef::as_ref)
    }

    pub fn child_mut(&mut self, index: usize) -> Option<&mut crate::component::DynComponent> {
        self.root_children_mut().get_mut(index).map(AsMut::as_mut)
    }

    /// The root children, which live in the alt screen's `implicitDocument`
    /// once it exists (`tui-alt-screen.ts:261-268`) and in the TUI itself
    /// otherwise. pi has exactly one list either way; where it is *stored*
    /// here is a Rust detail no caller may observe.
    /// Take the flat root children out, leaving the list empty. `take_root_children`
    /// stays an internal helper upstream (the chat viewport shares the tree by
    /// reference); the port's viewport has to *own* the hosts, so the fullscreen
    /// mount pulls them out this way.
    pub fn take_root_children(&mut self) -> Vec<Box<dyn crate::component::Component>> {
        std::mem::take(self.root_children_mut())
    }

    pub(crate) fn root_children(&self) -> &[Box<dyn crate::component::Component>] {
        match self
            .alt_screen
            .as_ref()
            .and_then(|alt| alt.implicit_document())
        {
            Some(children) => children,
            None => &self.children,
        }
    }

    pub(crate) fn root_children_mut(&mut self) -> &mut Vec<Box<dyn crate::component::Component>> {
        let implicit = self
            .alt_screen
            .as_mut()
            .and_then(|alt| alt.implicit_document_mut());
        match implicit {
            Some(children) => children,
            None => &mut self.children,
        }
    }

    /// `super.render(width)`, `tui-alt-screen.ts:328`: with no explicit
    /// layout root the document is the root children, concatenated.
    pub(crate) fn render_children(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for child in self.root_children() {
            lines.extend(child.render(width));
        }
        lines
    }

    pub fn terminal(&self) -> &dyn Terminal {
        self.terminal.as_ref()
    }

    /// Tests drive terminal size changes through this; hosts use
    /// [`Terminal::handle_resize`].
    pub fn terminal_mut(&mut self) -> &mut dyn Terminal {
        self.terminal.as_mut()
    }

    pub fn full_redraws(&self) -> usize {
        self.full_redraw_count
    }

    /// Count one more full redraw. The alt-screen painter keeps its own
    /// screen state but shares this counter, `tui-alt-screen.ts:1709`.
    pub(crate) fn record_full_redraw(&mut self) {
        self.full_redraw_count += 1;
    }

    /// The lines the last frame wrote. Test seam for the renderer.
    pub fn last_lines(&self) -> &[String] {
        &self.previous_lines
    }

    /// The bytes the last frame wrote.
    pub fn last_frame(&self) -> Option<&str> {
        self.last_frame.as_deref()
    }

    /// The last frame as a cell grid. `crate::frame` explains the seam.
    pub fn last_frame_grid(&self) -> CellGrid {
        let size = self.terminal.size().unwrap_or(TerminalSize {
            columns: 80,
            rows: 24,
        });
        crate::frame::cell_grid(
            &self.previous_lines,
            size.columns as usize,
            size.rows as usize,
        )
    }

    /// `tui.ts:570-581`.
    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.clear_on_shrink = enabled;
    }

    /// `tui.ts:561-568`.
    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        self.show_hardware_cursor = enabled;
    }

    /// `requestRender`, `tui.ts:990-999`. The host turns this into a frame
    /// through [`Tui::render_due_at`] / [`Tui::render_now`]: the request waits
    /// out [`Tui::MIN_RENDER_INTERVAL`], so a stream of deltas becomes one
    /// frame a terminal can show.
    pub fn request_render(&mut self) {
        self.render_requested = true;
    }

    /// `requestImmediateRender`, `tui.ts:1001-1017`. Keyboard input is
    /// latency-sensitive: pi avoids the throttled timer path, where even
    /// `setTimeout(0)` can take a full 16 ms tick on Windows, and renders on
    /// the next tick instead. This cancels the floor for the pending request.
    pub fn request_immediate_render(&mut self) {
        self.render_requested = true;
        self.immediate_render = true;
    }

    /// `requestRender(force)`, `tui.ts:990-995`. `true` resets the render
    /// state and asks for a frame that skips the floor; `false` is
    /// [`Tui::request_render`].
    pub fn request_render_with_force(&mut self, force: bool) {
        if force {
            self.reset_render_state();
            self.request_immediate_render();
        } else {
            self.request_render();
        }
    }

    pub fn render_requested(&self) -> bool {
        self.render_requested
    }

    /// `tui.ts:916-929`.
    pub fn start(&mut self) -> io::Result<()> {
        self.start_with(TuiStartOptions::default())
    }

    /// `Tui.start`, with pi's `mouse` opt-out. `tui-alt-screen.ts:274`.
    pub fn start_with(&mut self, options: TuiStartOptions) -> io::Result<()> {
        self.stopped = false;
        // The terminal writes the enable sequence in `start`, so the mode has
        // to be recorded first: `mouse: false` must suppress it entirely.
        self.terminal.request_mouse_mode(options.mouse);
        self.terminal.start()?;
        // pi re-raises SIGWINCH right after the terminal starts
        // (`terminal.ts:192-194`) because the real size can be stale after a
        // suspend, and the signal it raises is what the resize handler answers.
        self.terminal.refresh_size();
        // `tui.ts:925-927`: the mode is written after the cursor is hidden
        // and only when something asked for the reports.
        if self.color_scheme_notifications {
            self.terminal
                .write(crate::terminal_colors::COLOR_SCHEME_MODE_ON)?;
        }
        // `tui.ts:928` asks for the cell size on every start, right after the
        // cursor is hidden. It is one `CSI 16 t`, and `query_cell_size`
        // swallows it when the terminal draws no images.
        self.query_cell_size()?;
        self.request_render();
        Ok(())
    }

    /// pi's resize handler: `terminal.start(onInput, () => this.requestRender())`
    /// (`tui.ts:921`), with the size refresh node performs before it fires
    /// (`terminal.ts:190`).
    ///
    /// Without this the cached size stays whatever `start` found, so a window
    /// the user resized mid-session keeps laying out at its startup
    /// dimensions for the rest of the run.
    pub fn handle_resize(&mut self) {
        self.terminal.refresh_size();
        self.request_render();
    }

    /// `tui.ts:970-980`.
    pub fn stop(&mut self, options: TuiStopOptions) -> io::Result<()> {
        self.stopped = true;
        // `tui.ts:973-975`: the mode is dropped first, before the trailing
        // newline `beforeTerminalStop` writes.
        if self.color_scheme_notifications {
            self.terminal
                .write(crate::terminal_colors::COLOR_SCHEME_MODE_OFF)?;
        }
        // pi's renderer is one subclass or the other, and each owns both hooks:
        // `TuiAltScreen.beforeTerminalStop`/`afterTerminalStop`
        // (`:369-419`) are the alt screen's, `TuiMainScreen.beforeTerminalStop`
        // (`tui-main-screen.ts:168-176`) the main screen's. Dispatching here is
        // what makes `Tui::stop` leave the terminal the way the screen that
        // was using it has to be left.
        if self.alt_screen_active() {
            self.before_alt_screen_stop();
        } else {
            self.before_terminal_stop(options);
        }
        let result = self.terminal.stop();
        if self.alt_screen_active() {
            self.after_alt_screen_stop(options);
        }
        result
    }

    /// `setTerminalColorSchemeNotifications`, `tui.ts:950-958`. A repeated
    /// call writes nothing; a change writes the mode at once unless the TUI is
    /// stopped, where `start` and `stop` own it.
    pub fn set_color_scheme_notifications(&mut self, enabled: bool) {
        if self.color_scheme_notifications == enabled {
            return;
        }
        self.color_scheme_notifications = enabled;
        if !self.stopped {
            let _ = self.terminal.write(if enabled {
                crate::terminal_colors::COLOR_SCHEME_MODE_ON
            } else {
                crate::terminal_colors::COLOR_SCHEME_MODE_OFF
            });
        }
    }

    /// Whether the terminal was asked to report its colour scheme.
    pub fn color_scheme_notifications(&self) -> bool {
        self.color_scheme_notifications
    }

    /// `tui-main-screen.ts:168-176`.
    fn before_terminal_stop(&mut self, options: TuiStopOptions) {
        if options.preserve_screen || self.previous_lines.is_empty() {
            return;
        }
        let _ = self.terminal.write(" ");
        let target_row = self.previous_lines.len() as i64;
        let line_diff = target_row - self.hardware_cursor_row;
        if line_diff > 0 {
            let _ = self.terminal.write(&format!("\x1b[{line_diff}B"));
        } else if line_diff < 0 {
            let _ = self.terminal.write(&format!("\x1b[{}A", -line_diff));
        }
        let _ = self.terminal.write("\r\n");
    }

    /// `tui.ts:911-914`.
    pub fn invalidate(&mut self) {
        self.invalidate_components();
        self.reset_render_state();
    }

    /// Drop every component's own render cache and nothing else: the main
    /// screen keeps its geometry, so the next frame still sees the previous
    /// width and height and repaints fully instead of painting onto a screen
    /// that no longer exists.
    ///
    /// This is what pi's cell-size reply does (`tui.ts:1190-1193`): invalidate
    /// the children, request a render, leave the renderer's state alone.
    pub fn invalidate_components(&mut self) {
        for child in self.root_children_mut() {
            child.invalidate();
        }
        for overlay in &mut self.overlays {
            overlay.component.invalidate();
        }
    }

    /// One resize tick, host-driven. pi's `process.stdout.on("resize", …)`
    /// (`terminal.ts:190`) re-queries the dimensions and asks for a frame
    /// (`tui.ts:919-922`).
    ///
    /// `pi-tui` has no signal watcher, so the host delivers the tick and gets
    /// the re-measured size back to apply to whatever else tracks the
    /// viewport. The call is idempotent: a tick that changes nothing renders
    /// nothing.
    ///
    /// Cell pixel dimensions are deliberately *not* re-probed. pi caches them
    /// (`terminal-image.ts:41-47`) and only re-queries at start
    /// (`tui.ts:928`); a terminal that changes its cell size tells us with a
    /// CSI 6 reply, which [`Tui::consume_terminal_response`] already handles.
    pub fn apply_resize(&mut self) -> io::Result<TerminalSize> {
        self.terminal.refresh_size();
        let size = self.terminal.size()?;
        self.invalidate_components();
        self.request_render();
        Ok(size)
    }

    /// `tui-main-screen.ts:158-166`.
    pub(crate) fn reset_render_state(&mut self) {
        self.previous_lines.clear();
        self.previous_width = -1;
        self.previous_height = -1;
        self.cursor_row = 0;
        self.hardware_cursor_row = 0;
        self.max_lines_rendered = 0;
        self.previous_viewport_top = 0;
    }

    // ---------------------------------------------------------------- focus

    /// `tui.ts:583-586`.
    pub fn focused_component(&self) -> Option<usize> {
        self.focused_component
    }

    /// The focus handle. `tui.ts:583-586`.
    pub fn focused_target(&self) -> Option<FocusTarget> {
        self.focused
    }

    /// The nested-overlay focus-restore state. `tui.ts:329-337`.
    pub fn overlay_focus_restore_state(&self) -> OverlayFocusRestoreState {
        self.overlay_focus_restore
    }

    /// The most recent bytes written to the terminal. Tests assert query bytes
    /// with it; production never calls it.
    pub fn last_written(&self) -> Option<String> {
        self.terminal.last_written()
    }

    /// `tui.ts:587-589` and `setFocusInternal`.
    pub fn set_focus(&mut self, target: Option<FocusTarget>) {
        self.set_focus_with_policy(target, OverlayFocusRestorePolicy::Preserve);
    }

    /// [`Tui::set_focus`] with the restore policy spelled out. `tui.ts:591-696`.
    ///
    /// A focus change that leaves the focused overlay for something which is
    /// not inside it is *blocked*: the overlay is remembered so that when the
    /// child goes away focus can go back. That is what stops a closed settings
    /// submenu from stranding focus on a detached component.
    pub fn set_focus_with_policy(
        &mut self,
        target: Option<FocusTarget>,
        policy: OverlayFocusRestorePolicy,
    ) {
        let previous_focus = self.focused_id;
        let mut next_focus = target;
        let previous_focused_overlay = previous_focus.and_then(|id| self.visible_overlay_of(id));
        let next_focus_is_overlay = next_focus.is_some_and(
            |target| matches!(target, FocusTarget::Overlay(id) if self.find_overlay(id).is_some()),
        );
        let restore_state = self.visible_overlay_focus_restore();

        let next_id = self.id_of(next_focus);
        if next_focus.is_some() && !next_focus_is_overlay {
            match restore_state {
                OverlayFocusRestoreState::Blocked {
                    overlay,
                    blocked_by,
                    resume,
                } if Some(blocked_by) == previous_focus => {
                    if matches!(resume, OverlayFocusResume::FocusTarget(_))
                        || !self.is_component_mounted(blocked_by)
                    {
                        next_focus = self.resolve_blocked_focus_resume(overlay, resume);
                    } else {
                        self.overlay_focus_restore = OverlayFocusRestoreState::Blocked {
                            overlay,
                            blocked_by: next_id,
                            resume,
                        };
                    }
                }
                OverlayFocusRestoreState::Eligible { overlay } => {
                    if let Some(focused_overlay) = previous_focused_overlay {
                        if overlay == focused_overlay
                            && !self.is_overlay_focus_ancestor(
                                focused_overlay,
                                next_focus.map(|_| next_id),
                            )
                        {
                            self.overlay_focus_restore = OverlayFocusRestoreState::Blocked {
                                overlay: focused_overlay,
                                blocked_by: next_id,
                                resume: OverlayFocusResume::RestoreOverlay,
                            };
                        }
                    }
                }
                OverlayFocusRestoreState::Inactive | OverlayFocusRestoreState::Blocked { .. } => {}
            }
        } else if next_focus.is_none() {
            if let OverlayFocusRestoreState::Blocked {
                overlay,
                blocked_by,
                resume,
            } = restore_state
            {
                if Some(blocked_by) == previous_focus {
                    next_focus = self.resolve_blocked_focus_resume(overlay, resume);
                }
            } else if policy == OverlayFocusRestorePolicy::Clear {
                self.clear_overlay_focus_restore();
            }
        }

        if let Some(previous) = previous_focus {
            if let Some(component) = self.component_mut(previous) {
                set_focus_flag(component, false);
            }
        }

        self.focused = next_focus;
        self.focused_component = None;
        self.focused_id = next_focus.map(|target| self.target_id(target));
        if let Some(next) = self.focused_id {
            if let Some(component) = self.component_mut(next) {
                set_focus_flag(component, true);
            }
        }
        if let Some(FocusTarget::Root(index)) = next_focus {
            self.focused_component = Some(index);
        }

        if let Some(overlay) = self.focused_id.and_then(|id| self.visible_overlay_of(id)) {
            self.overlay_focus_restore = OverlayFocusRestoreState::Eligible { overlay };
        }
    }

    /// The component identity behind a focus handle. `tui.ts:597`.
    fn target_id(&self, target: FocusTarget) -> ComponentId {
        match target {
            FocusTarget::Root(index) => self
                .root_children()
                .get(index)
                .map(|child| component_id(child.as_ref())),
            FocusTarget::Overlay(id) => self
                .find_overlay(id)
                .and_then(|index| self.overlays.get(index))
                .map(|entry| component_id(entry.component.as_ref())),
            FocusTarget::Id(id) => Some(id),
        }
        .unwrap_or(0)
    }

    fn id_of(&self, target: Option<FocusTarget>) -> ComponentId {
        target.map_or(0, |target| self.target_id(target))
    }

    /// Whether `id` is the alt screen's `implicitScrollView` or its
    /// `implicitDocument` (`:261-268`). Both are mounted whenever the alt
    /// screen has been entered, even though neither is a root child: they are
    /// the layout root the host's own children hang off, and the mouse walk
    /// has to reach both.
    fn implicit_root_holds(&self, id: ComponentId) -> bool {
        let Some(root) = self.alt().and_then(|alt| alt.implicit_root.as_deref()) else {
            return false;
        };
        component_id(root) == id || component_id(root.child()) == id
    }

    fn component_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if self.implicit_root_holds(id) {
            let root = self
                .alt_screen
                .as_mut()
                .and_then(|alt| alt.implicit_root.as_deref_mut())?;
            if component_id(&*root) == id {
                return Some(root);
            }
            return Some(root.child_mut());
        }
        let index = self
            .root_children()
            .iter()
            .position(|child| component_id(child.as_ref()) == id);
        if let Some(index) = index {
            let child = self.root_children_mut().get_mut(index)?;
            return Some(&mut **child);
        }
        let overlay_index = self
            .overlays
            .iter()
            .position(|entry| component_id(entry.component.as_ref()) == id);
        if let Some(index) = overlay_index {
            return Some(self.overlays[index].component.as_mut());
        }
        // The fullscreen chat viewport mounts the host's flat components
        // inside its own tree (upstream shares them by reference), so a
        // nested component — the editor in the dock, a selector in the
        // editor's slot — is only reachable by walking layout children.
        // Direct field access keeps the two walks disjoint to borrowck.
        let nested: Option<&mut Vec<Box<dyn Component>>> = match self.alt_screen.as_mut() {
            Some(alt) => alt.implicit_document_mut(),
            None => Some(&mut self.children),
        };
        if let Some(children) = nested {
            for child in children.iter_mut() {
                if let Some(found) = Self::component_mut_deep(&mut **child, id) {
                    return Some(found);
                }
            }
        }
        for entry in &mut self.overlays {
            if let Some(found) = Self::component_mut_deep(&mut *entry.component, id) {
                return Some(found);
            }
        }
        None
    }

    /// Depth-first search for `id` through a subtree, following layout
    /// children (containers, stacks, scroll views). The name-by-identity
    /// counterpart of upstream's direct object references.
    fn component_mut_deep(
        component: &mut crate::component::DynComponent,
        id: ComponentId,
    ) -> Option<&mut dyn Component> {
        // `component` is already `&mut dyn Component`; identity and return
        // need no downcast.
        if component_id(component) == id {
            return Some(component);
        }
        for child in component.layout_children_mut() {
            if let Some(found) = Self::component_mut_deep(child, id) {
                return Some(found);
            }
        }
        None
    }

    /// Mutably borrow a mounted component by identity, at any depth. The
    /// host-facing form of the deep walk that backs
    /// [`FocusTarget::Id`] delivery.
    pub fn component_by_id_deep_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        self.component_mut(id)
    }

    /// `isComponentMounted`, `tui.ts:669-679`: the component is a root child or
    /// a live overlay.
    fn is_component_mounted(&self, id: ComponentId) -> bool {
        self.root_children()
            .iter()
            .any(|child| component_id(child.as_ref()) == id)
            || self
                .overlays
                .iter()
                .any(|entry| component_id(entry.component.as_ref()) == id)
    }

    /// The visible overlay whose component is `id`. `tui.ts:602-604`.
    fn visible_overlay_of(&self, id: ComponentId) -> Option<OverlayId> {
        self.overlays
            .iter()
            .find(|entry| {
                component_id(entry.component.as_ref()) == id && self.is_overlay_visible(entry)
            })
            .map(OverlayEntry::id)
    }

    /// `resolveBlockedOverlayFocusResume`, `tui.ts:664-668`.
    fn resolve_blocked_focus_resume(
        &mut self,
        overlay: OverlayId,
        resume: OverlayFocusResume,
    ) -> Option<FocusTarget> {
        match resume {
            OverlayFocusResume::RestoreOverlay => Some(FocusTarget::Overlay(overlay)),
            OverlayFocusResume::FocusTarget(target) => {
                self.clear_overlay_focus_restore();
                target
            }
        }
    }

    /// `getVisibleOverlayFocusRestore`, `tui.ts:670-676`: a state naming an
    /// overlay that is gone or hidden is treated as inactive.
    fn visible_overlay_focus_restore(&self) -> OverlayFocusRestoreState {
        match self.overlay_focus_restore {
            OverlayFocusRestoreState::Eligible { overlay } if self.overlay_is_live(overlay) => {
                OverlayFocusRestoreState::Eligible { overlay }
            }
            OverlayFocusRestoreState::Blocked {
                overlay,
                blocked_by,
                resume,
            } if self.overlay_is_live(overlay) => OverlayFocusRestoreState::Blocked {
                overlay,
                blocked_by,
                resume,
            },
            _ => OverlayFocusRestoreState::Inactive,
        }
    }

    fn overlay_is_live(&self, overlay: OverlayId) -> bool {
        self.find_overlay(overlay)
            .is_some_and(|index| self.is_overlay_visible(&self.overlays[index]))
    }

    /// `clearOverlayFocusRestore`, `tui.ts:661-663`.
    pub fn clear_overlay_focus_restore(&mut self) {
        self.overlay_focus_restore = OverlayFocusRestoreState::Inactive;
    }

    /// `clearOverlayFocusRestoreFor`, `tui.ts:665-669`.
    fn clear_overlay_focus_restore_for(&mut self, overlay: OverlayId) {
        if matches!(self.overlay_focus_restore, OverlayFocusRestoreState::Eligible { overlay: id } | OverlayFocusRestoreState::Blocked { overlay: id, .. } if id == overlay)
        {
            self.clear_overlay_focus_restore();
        }
    }

    /// `isOverlayFocusAncestor`, `tui.ts:684-695`. Walks the pre-focus chain
    /// from `entry`, cycle-safe: a malformed chain must not hang the UI.
    fn is_overlay_focus_ancestor(&self, entry: OverlayId, component: Option<ComponentId>) -> bool {
        let Some(component) = component else {
            return false;
        };
        let mut visited: Vec<ComponentId> = Vec::new();
        let mut current = self
            .find_overlay(entry)
            .and_then(|index| self.overlays[index].pre_focus_id);
        while let Some(id) = current {
            if id == component {
                return true;
            }
            // A pre-focus chain that revisits an overlay would loop forever.
            if visited.contains(&id) {
                return false;
            }
            visited.push(id);
            current = self
                .overlays
                .iter()
                .find(|overlay| component_id(overlay.component.as_ref()) == id)
                .and_then(|overlay| overlay.pre_focus_id);
        }
        false
    }

    /// `tui.ts:891-897`.
    fn is_overlay_visible(&self, entry: &OverlayEntry) -> bool {
        if entry.hidden {
            return false;
        }
        match &entry.options.visible {
            Some(visible) => {
                let size = self.terminal.size().unwrap_or(TerminalSize {
                    columns: 80,
                    rows: 24,
                });
                visible(size.columns, size.rows)
            }
            None => true,
        }
    }

    /// `tui.ts:900-909`.
    fn topmost_visible_overlay(&self) -> Option<usize> {
        let mut topmost: Option<usize> = None;
        for (index, overlay) in self.overlays.iter().enumerate() {
            if overlay.options.non_capturing || !self.is_overlay_visible(overlay) {
                continue;
            }
            if topmost
                .is_none_or(|current| overlay.focus_order > self.overlays[current].focus_order)
            {
                topmost = Some(index);
            }
        }
        topmost
    }

    // --------------------------------------- seam for the alt-screen renderer
    //
    // `tui-alt-screen.ts` is a subclass of the base TUI in pi, so it reaches
    // the overlay stack, the focus owner and the terminal directly. Rust keeps
    // those fields private to this module, so the alt screen gets the narrow
    // `pub(crate)` surface it actually uses instead of `pub` fields.

    /// Write straight to the terminal. Every escape the alt screen emits goes
    /// through here so a test can capture it.
    pub(crate) fn write_terminal(&mut self, text: &str) {
        let _ = self.terminal.write(text);
    }

    /// The visible overlays that have rendered bounds, topmost first.
    /// `dispatchMouseToOverlay`, `tui.ts:861-882`.
    pub(crate) fn rendered_overlays_topmost_first(
        &self,
    ) -> Vec<(OverlayId, OverlayBounds, ComponentId)> {
        let mut out: Vec<(OverlayId, OverlayBounds, ComponentId)> = self
            .overlays
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.options.non_capturing && self.is_overlay_visible(entry))
            .filter_map(|(_, entry)| {
                entry
                    .bounds
                    .map(|bounds| (entry.id(), bounds, component_id(entry.component.as_ref())))
            })
            .collect();
        out.reverse();
        out
    }

    /// `resolveMouseFocusTarget`, `tui.ts:850-860`: a visible overlay that
    /// hosts the clicked component keeps keyboard focus, so removing the child
    /// does not strand it.
    pub(crate) fn resolve_mouse_focus_target(&self, component: ComponentId) -> ComponentId {
        for entry in self.overlays.iter().rev() {
            if self.is_overlay_visible(entry)
                && self.is_overlay_focus_ancestor(entry.id(), Some(component))
            {
                return component_id(entry.component.as_ref());
            }
        }
        component
    }

    /// Give the keyboard to a component by identity. Returns `false` when the
    /// component is not mounted.
    pub(crate) fn set_focus_by_id(&mut self, id: ComponentId) -> bool {
        for (index, child) in self.root_children().iter().enumerate() {
            if component_id(child.as_ref()) == id {
                self.set_focus(Some(FocusTarget::Root(index)));
                return true;
            }
        }
        for entry in &self.overlays {
            if component_id(entry.component.as_ref()) == id {
                self.set_focus(Some(FocusTarget::Overlay(entry.id())));
                return true;
            }
        }
        // A nested component (fullscreen dock) can only be focused by
        // identity; the deep walk in `component_mut` verifies it is mounted.
        if self.component_mut(id).is_some() {
            self.set_focus(Some(FocusTarget::Id(id)));
            return true;
        }
        false
    }

    /// The identity of the component that owns the keyboard, or 0.
    pub(crate) fn focused_id_value(&self) -> ComponentId {
        self.focused_id.unwrap_or(0)
    }

    /// Dispatch to a mounted component by identity. `dispatchMouseEvent`.
    pub(crate) fn dispatch_mouse_by_id(
        &mut self,
        id: ComponentId,
        event: &crate::mouse::MouseEvent,
    ) -> Option<crate::mouse::MouseDispatchResult> {
        crate::mouse::dispatch_mouse_event(self.component_mut(id)?, event)
    }

    /// Read a mounted component by identity.
    pub(crate) fn component_by_id(&self, id: ComponentId) -> Option<&dyn Component> {
        if self.implicit_root_holds(id) {
            let root = self.alt()?.implicit_root.as_deref()?;
            if component_id(root) == id {
                return Some(root);
            }
            return Some(root.child());
        }
        if let Some(child) = self
            .root_children()
            .iter()
            .find(|child| component_id(child.as_ref()) == id)
        {
            return Some(child.as_ref());
        }
        self.overlays
            .iter()
            .find(|entry| component_id(entry.component.as_ref()) == id)
            .map(|entry| entry.component.as_ref())
    }

    /// Mutably borrow a mounted component by identity.
    pub(crate) fn component_by_id_mut(&mut self, id: ComponentId) -> Option<&mut dyn Component> {
        if let Some(index) = self
            .root_children()
            .iter()
            .position(|child| component_id(child.as_ref()) == id)
        {
            return Some(self.root_children_mut()[index].as_mut());
        }
        let entry = self
            .overlays
            .iter_mut()
            .find(|entry| component_id(entry.component.as_ref()) == id)?;
        Some(entry.component.as_mut())
    }

    /// pi's `handleMouse === Container.prototype.handleMouse` test
    /// (`tui-alt-screen.ts:840`): a component that is only a layout node is
    /// skipped by the layout walk, because the band walk beneath it already
    /// routes the event.
    pub(crate) fn component_is_layout_only(&mut self, id: ComponentId) -> bool {
        let Some(component) = self.component_mut(id) else {
            return true;
        };
        component.layout_node().is_some() && !component.overrides_mouse_handler()
    }

    /// Downcast a mounted component to a concrete type. The tree's own
    /// containers opt in (`ScrollView`, the stacks); leaves keep the default
    /// `None` unless they need host-side downcasting.
    pub(crate) fn component_any_mut(&mut self, id: ComponentId) -> Option<&mut dyn std::any::Any> {
        self.component_mut(id)?.as_any_mut()
    }

    /// Mutate an overlay's component. `None` when the id is stale.
    pub(crate) fn overlay_component_mut(
        &mut self,
        id: OverlayId,
    ) -> Option<&mut crate::component::DynComponent> {
        let index = self.find_overlay(id)?;
        Some(self.overlays[index].component.as_mut())
    }

    /// The terminal's reported geometry, or pi's 80x24 fallback.
    pub(crate) fn terminal_size_inner(&self) -> (usize, usize) {
        match self.terminal.size() {
            Ok(size) => (usize::from(size.columns), usize::from(size.rows)),
            Err(_) => (80, 24),
        }
    }

    /// The primary scroll view's identity from the last alt-screen layout
    /// pass. The host records it after laying the frame out, because the
    /// layout engine measures and mutates in two passes
    /// (`layout.rs:apply_scroll_updates`).
    pub(crate) fn primary_scroll_view_id(&self) -> Option<ComponentId> {
        self.alt_primary_scroll_view
    }

    /// Record the primary scroll view after an alt-screen layout pass.
    pub(crate) fn set_primary_scroll_view(&mut self, id: Option<ComponentId>) {
        self.alt_primary_scroll_view = id;
    }

    /// The terminal width, clamped to at least one column.
    pub(crate) fn terminal_width(&self) -> usize {
        self.terminal
            .size()
            .map_or(80, |size| usize::from(size.columns))
            .max(1)
    }

    /// Replace the root slot, returning the new component's identity.
    /// `setLayoutRoot`, `tui-alt-screen.ts:320-326`.
    pub(crate) fn replace_root_child(&mut self, component: Box<dyn Component>) -> ComponentId {
        let id = component_id(component.as_ref());
        if let Some(index) = self.alt_root {
            self.root_children_mut()[index] = component;
        } else {
            self.root_children_mut().push(component);
            self.alt_root = Some(self.root_children().len() - 1);
        }
        id
    }

    // -------------------------------------------------------------- overlay

    /// `tui.ts:718-732`.
    pub fn show_overlay(
        &mut self,
        component: Box<dyn Component>,
        options: Option<OverlayOptions>,
    ) -> OverlayId {
        self.focus_order_counter += 1;
        let entry = OverlayEntry {
            pre_focus_id: self.focused_id,
            component,
            options: options.unwrap_or_default(),
            pre_focus: self.focused,
            hidden: false,
            focus_order: self.focus_order_counter,
            bounds: None,
            generation: self.focus_order_counter,
        };
        let id = OverlayId {
            index: self.overlays.len(),
            generation: entry.generation,
        };
        let capturing = !entry.options.non_capturing;
        self.overlays.push(entry);
        if capturing && self.is_overlay_visible(self.overlays.last().expect("just pushed")) {
            self.set_focus(Some(FocusTarget::Overlay(id)));
        }
        self.request_render();
        id
    }

    /// `tui.ts:739-750`.
    pub fn hide_overlay(&mut self, id: OverlayId) {
        let Some(index) = self.find_overlay(id) else {
            return;
        };
        self.clear_overlay_focus_restore_for(id);
        let pre_focus = self.overlays[index].pre_focus;
        let pre_focus_id = self.overlays[index].pre_focus_id;
        let removed = self.overlays.remove(index);
        // `retargetOverlayPreFocus` (`tui.ts:698-703`): an overlay that was
        // focused before the removed one inherits its own pre-focus, so the
        // chain does not point at a dead component.
        for entry in &mut self.overlays {
            if entry.pre_focus_id == Some(component_id(removed.component.as_ref())) {
                entry.pre_focus = pre_focus;
                entry.pre_focus_id = pre_focus_id;
            }
        }
        // Indices shifted: every id above the removed one now points elsewhere.
        Self::reindex_overlays();
        if self.focused == Some(FocusTarget::Overlay(id)) {
            let target = self
                .topmost_visible_overlay()
                .map(|index| FocusTarget::Overlay(self.overlays[index].id()))
                .or(pre_focus);
            self.set_focus(target);
        }
        self.request_render();
    }

    /// `tui.ts:751-770`.
    pub fn set_overlay_hidden(&mut self, id: OverlayId, hidden: bool) {
        let Some(index) = self.find_overlay(id) else {
            return;
        };
        if self.overlays[index].hidden == hidden {
            return;
        }
        self.overlays[index].hidden = hidden;
        let id_current = self.overlays[index].id();
        let pre_focus = self.overlays[index].pre_focus;
        if hidden {
            if self.focused == Some(FocusTarget::Overlay(id_current)) {
                let target = self
                    .topmost_visible_overlay()
                    .map(|index| FocusTarget::Overlay(self.overlays[index].id()))
                    .or(pre_focus);
                self.set_focus(target);
            }
        } else if !self.overlays[index].options.non_capturing
            && self.is_overlay_visible(&self.overlays[index])
        {
            self.focus_order_counter += 1;
            self.overlays[index].focus_order = self.focus_order_counter;
            self.set_focus(Some(FocusTarget::Overlay(id_current)));
        }
        self.request_render();
    }

    /// `tui.ts:773-777`.
    pub fn focus_overlay(&mut self, id: OverlayId) {
        let Some(index) = self.find_overlay(id) else {
            return;
        };
        if !self.is_overlay_visible(&self.overlays[index]) {
            return;
        }
        self.focus_order_counter += 1;
        self.overlays[index].focus_order = self.focus_order_counter;
        self.set_focus(Some(FocusTarget::Overlay(self.overlays[index].id())));
        self.request_render();
    }

    /// The last rendered bounds of a visible overlay. `tui.ts:310-312`.
    pub fn overlay_bounds(&self, id: OverlayId) -> Option<OverlayBounds> {
        let index = self.find_overlay(id)?;
        self.overlays[index].bounds
    }

    // ------------------------------------------------- terminal queries (A.4)

    /// `queryTerminalColors`, `tui.ts:478-484`, `:1470-1493`.
    ///
    /// Writes OSC 10/11 and OSC 4 for all sixteen palette entries, fenced by a
    /// primary device attributes request. `timeout_ms` is a deadline the host
    /// services in [`Tui::service_terminal_queries`] — this crate owns no
    /// timer. `timeout_ms` is that deadline as a point on the host clock, not
    /// pi's relative `timeoutMs`: the host owns the clock here, so it hands
    /// over the instant the query expires.
    ///
    /// `on_late_reply` is this query's `onLateReply`, taken as an argument
    /// exactly as pi takes it (`tui.ts:1481-1487`), so two queries in flight
    /// have two sinks and a late reply reaches the query that asked for it.
    /// It is armed when [`Tui::service_terminal_queries`] sees this query's
    /// deadline pass, and fires with the colours the terminal sent after it,
    /// which is what makes the theme usable over a slow link.
    pub fn query_terminal_colors(
        &mut self,
        timeout_ms: u64,
        on_late_reply: Option<Box<dyn FnMut(TerminalColors) + Send>>,
    ) -> io::Result<()> {
        self.pending_color_queries.push(PendingTerminalColorQuery {
            palette: [None; TERMINAL_PALETTE_SIZE],
            foreground: None,
            background: None,
            replied: vec![false; TERMINAL_COLOR_REPLY_COUNT],
            replied_count: 0,
            deadline_ms: timeout_ms,
            delivery: TerminalColorDelivery::Resolve,
            late_sink: on_late_reply,
        });
        self.terminal.write(&terminal_color_query())
    }

    /// Drain buffered replies. `tui.ts:1069-1071`, `:1122-1200`.
    ///
    /// Returns true when the reply belonged to a terminal query and must not
    /// reach the keyboard handler.
    pub fn consume_terminal_response(&mut self, data: &str) -> bool {
        if self.consume_cell_size_response(data) {
            return true;
        }
        if self.consume_color_scheme_report(data) {
            return true;
        }
        self.consume_terminal_color_response(data)
    }

    /// Pull whatever the terminal has buffered. Hosts call this instead of
    /// [`crate::terminal::Terminal::poll_response`] so query replies reach the
    /// state machine.
    pub fn poll_terminal_response(&mut self) -> bool {
        match self.terminal.poll_response() {
            Some(response) => {
                self.consume_terminal_response(&response);
                true
            }
            None => false,
        }
    }

    /// `consumeTerminalColorResponse`, `tui.ts:1122-1165`.
    fn consume_terminal_color_response(&mut self, data: &str) -> bool {
        // A palette index past the 16 the query asked for is answered by some
        // terminals anyway; it is swallowed without counting towards the
        // completion condition (`tui.ts:1144-1149`).
        const BACKGROUND_SLOT: usize = TERMINAL_COLOR_REPLY_COUNT - 1;
        const FOREGROUND_SLOT: usize = TERMINAL_COLOR_REPLY_COUNT - 2;
        if self.pending_color_queries.is_empty() {
            return false;
        }
        if is_device_attributes_response(data) {
            let mut query = self.pending_color_queries.remove(0);
            self.complete_terminal_color_query(&mut query);
            return true;
        }
        let Some((target, rgb)) = parse_osc_color_response(data) else {
            return false;
        };
        let slot = match target {
            OscColorTarget::Foreground => None,
            OscColorTarget::Background => Some(BACKGROUND_SLOT),
            OscColorTarget::Palette(index) if index < TERMINAL_PALETTE_SIZE => Some(index),
            OscColorTarget::Palette(_) => return true,
        };
        let Some(query) = self.pending_color_queries.first_mut() else {
            return true;
        };
        // `tui.ts:1132-1133`: a query whose `deliver` is unset keeps eating
        // replies but records none.
        let key = slot.unwrap_or(FOREGROUND_SLOT);
        if !query.delivers() || query.replied[key] {
            return true;
        }
        query.replied[key] = true;
        query.replied_count += 1;
        match slot {
            None => query.foreground = rgb,
            Some(BACKGROUND_SLOT) => query.background = rgb,
            Some(index) => query.palette[index] = rgb,
        }
        if query.replied_count == TERMINAL_COLOR_REPLY_COUNT {
            // pi completes the query but leaves it queued; the DA1 fence that
            // follows is what shifts it off (`tui.ts:1150-1152`, `:1127-1131`).
            let mut query = self.pending_color_queries.remove(0);
            self.complete_terminal_color_query(&mut query);
            self.pending_color_queries.insert(0, query);
        }
        true
    }

    /// `terminalColorQueryResult`, `tui.ts:1156-1160`: the palette is only
    /// reported when all sixteen arrived, because a partial palette would
    /// paint a half-resolved theme.
    fn terminal_color_query_result(query: &PendingTerminalColorQuery) -> TerminalColors {
        let palette = if query.palette.iter().all(Option::is_some) {
            let mut colours = [RgbColor { r: 0, g: 0, b: 0 }; TERMINAL_PALETTE_SIZE];
            for (slot, colour) in colours.iter_mut().zip(query.palette.iter()) {
                *slot = colour.expect("all slots filled");
            }
            Some(colours)
        } else {
            None
        };
        TerminalColors {
            foreground: query.foreground,
            background: query.background,
            palette,
        }
    }

    /// `completeTerminalColorQuery`, `tui.ts:1162-1166`: read pi's `deliver`
    /// once, unset it so a later completion delivers nothing, and hand the
    /// result to wherever it was pointing. Before the deadline that is the
    /// promise ([`Tui::terminal_color_results`]); after it, the query's own
    /// `onLateReply` sink.
    fn complete_terminal_color_query(&mut self, query: &mut PendingTerminalColorQuery) {
        let colors = Self::terminal_color_query_result(query);
        match query.delivery {
            TerminalColorDelivery::Resolve => {
                query.delivery = TerminalColorDelivery::Done;
                self.terminal_color_results.push(colors);
            }
            TerminalColorDelivery::Late => {
                query.delivery = TerminalColorDelivery::Done;
                if let Some(mut sink) = query.late_sink.take() {
                    sink(colors);
                }
            }
            TerminalColorDelivery::Done => {}
        }
    }

    /// The colours of a completed query, for hosts that prefer to drain the
    /// queue than to pass a sink.
    pub fn take_terminal_colors(&mut self) -> Option<TerminalColors> {
        if self.terminal_color_results.is_empty() {
            return None;
        }
        Some(self.terminal_color_results.remove(0))
    }

    /// pi's timeout arm (`tui.ts:1481-1484`): settle every query whose
    /// deadline has passed with the colours collected so far, and re-arm it
    /// for late replies. This crate owns no timer, so the host calls this
    /// with its own clock.
    ///
    /// The late slot belongs to the query: each query keeps its own
    /// [`PendingTerminalColorQuery::late_sink`], and only the query whose
    /// deadline fired has its `deliver` swapped, so a second query in flight
    /// keeps resolving through [`Tui::take_terminal_colors`] until its own
    /// deadline passes.
    ///
    /// Returns the colours each deadline settled, oldest query first. A query
    /// that already completed is not settled twice — pi clears its timer in
    /// `completeTerminalColorQuery` (`tui.ts:1164`).
    pub fn service_terminal_queries(&mut self, now_ms: u64) -> Vec<TerminalColors> {
        let expired: Vec<usize> = self
            .pending_color_queries
            .iter()
            .enumerate()
            .filter(|(_, query)| {
                query.delivery == TerminalColorDelivery::Resolve && now_ms >= query.deadline_ms
            })
            .map(|(index, _)| index)
            .collect();
        let mut settled = Vec::with_capacity(expired.len());
        for index in expired {
            let query = &mut self.pending_color_queries[index];
            // `query.deliver = onLateReply; resolve(...)` — the swap first, and
            // an absent `onLateReply` leaves `deliver` undefined.
            query.delivery = if query.late_sink.is_some() {
                TerminalColorDelivery::Late
            } else {
                TerminalColorDelivery::Done
            };
            let colors = Self::terminal_color_query_result(query);
            self.terminal_color_results.push(colors);
            settled.push(colors);
        }
        settled
    }

    /// `consumeTerminalColorSchemeReport`, `tui.ts:1167-1180`.
    fn consume_color_scheme_report(&mut self, data: &str) -> bool {
        let Some(scheme) = parse_terminal_color_scheme_report(data) else {
            return false;
        };
        self.pending_color_scheme_reports.push(scheme);
        self.dispatch_color_scheme(scheme);
        true
    }

    /// Take the colour-scheme reports received since the last call.
    ///
    /// pi has no queue: `tui.ts:1174-1176` notifies every registered listener
    /// and returns. This drain stays for hosts that prefer polling to
    /// registering; a host that does both sees each report twice.
    pub fn take_color_scheme_reports(&mut self) -> Vec<TerminalColorScheme> {
        std::mem::take(&mut self.pending_color_scheme_reports)
    }

    /// `tui.ts:943-948` (`onTerminalColorSchemeChange`). pi returns an
    /// unsubscribe closure; this port returns the id that
    /// [`Tui::remove_color_scheme_listener`] takes, and
    /// [`Tui::color_scheme_listener_registry`] hands out the same removal for
    /// a caller that is not holding `&mut Tui` — a listener removing itself
    /// from inside its own call.
    ///
    /// pi's live `Set` (`tui.ts:943-948`) dedupes an identical listener
    /// reference, so registering one function twice calls it once. The
    /// listener is an [`Arc`] for that reason: two clones of one
    /// [`ColorSchemeListener`] are one `Set` entry, and the second
    /// registration hands back the id that already names it instead of
    /// storing a duplicate. Registering two separately built closures is two
    /// entries, as in pi.
    pub fn add_color_scheme_listener(
        &mut self,
        listener: ColorSchemeListener,
    ) -> ColorSchemeListenerId {
        if let Some(generation) = self.color_scheme_listeners.generation_of(&listener) {
            return ColorSchemeListenerId { generation };
        }
        self.color_scheme_listener_counter += 1;
        let id = ColorSchemeListenerId {
            generation: self.color_scheme_listener_counter,
        };
        self.color_scheme_listeners
            .set()
            .push(ColorSchemeListenerEntry {
                generation: id.generation,
                listener: Some(listener),
            });
        id
    }

    /// Drops a listener added by [`Tui::add_color_scheme_listener`]. pi's
    /// unsubscribe closure removes exactly the listener that made it
    /// (`tui.ts:945-947`); an id that already fired removes nothing.
    pub fn remove_color_scheme_listener(&mut self, id: ColorSchemeListenerId) {
        self.color_scheme_listeners.remove(id);
    }

    /// pi's unsubscribe closure (`tui.ts:945-947`) as a value a listener can
    /// hold. The id from [`Tui::add_color_scheme_listener`] only `&mut self`
    /// can redeem, and the dispatch holds that borrow, so a listener could not
    /// delete itself while it ran. A clone of this registry addresses the same
    /// list from anywhere, a call from inside a listener included.
    ///
    /// The id only exists once the listener is registered, so a self-removing
    /// listener reads it back through a cell the host fills in afterwards —
    /// the Rust shape of a closure that deletes itself by identity.
    pub fn color_scheme_listener_registry(&self) -> ColorSchemeListenerRegistry {
        self.color_scheme_listeners.clone()
    }

    /// `for (const listener of this.terminalColorSchemeListeners)
    /// listener(scheme)` (`tui.ts:1174-1176`) over a set that may be edited
    /// while it is walked: an entry deleted before the cursor reaches it is
    /// skipped, one deleted after it ran is not revisited, and one registered
    /// during the walk is visited.
    fn dispatch_color_scheme(&self, scheme: TerminalColorScheme) {
        let mut index = 0;
        while let Some((generation, listener)) = self.take_color_scheme_listener(index) {
            listener(scheme);
            // Advance only when the entry that just ran is still the one under
            // the cursor. If it — or anything before it — was removed, the
            // next listener has shifted down into this slot and stepping over
            // it would skip a listener pi would have called.
            if self.restore_color_scheme_listener(index, generation, listener) {
                index += 1;
            }
        }
    }

    /// Moves the listener at `index` out for the call and leaves the slot
    /// empty, so no lock is held while it runs. `None` at the end of the list,
    /// or in a slot another walk emptied.
    fn take_color_scheme_listener(&self, index: usize) -> Option<TakenColorSchemeListener> {
        let mut set = self.color_scheme_listeners.set();
        let entry = set.get_mut(index)?;
        Some((entry.generation, entry.listener.take()?))
    }

    /// Puts the listener back and reports whether the cursor may advance. The
    /// slot is empty when the entry was removed while the listener ran: that
    /// is pi's `Set.delete`, and dropping the closure here completes it.
    fn restore_color_scheme_listener(
        &self,
        index: usize,
        generation: u64,
        listener: ColorSchemeListener,
    ) -> bool {
        let mut set = self.color_scheme_listeners.set();
        match set.get_mut(index) {
            Some(entry) if entry.generation == generation => {
                entry.listener = Some(listener);
                true
            }
            _ => false,
        }
    }

    /// `consumeCellSizeResponse`, `tui.ts:1180-1196`.
    ///
    /// A report is eaten whatever it carries: pi returns `true` for a
    /// zero-sized reply rather than letting it reach a component.
    fn consume_cell_size_response(&mut self, data: &str) -> bool {
        if !is_cell_size_response(data) {
            return false;
        }
        let Some((height_px, width_px)) = parse_cell_size_response(data) else {
            return true;
        };
        crate::terminal_image::set_cell_dimensions(crate::terminal_image::CellDimensions {
            width_px,
            height_px,
        });
        // Images re-render against the new cell size, so every cached layout
        // is stale (`tui.ts:1192-1194`).
        self.invalidate();
        self.request_render();
        true
    }

    /// `queryCellSize`, `tui.ts:960-970`. Only worth asking when the terminal
    /// draws images: the cell size is used for nothing else.
    pub fn query_cell_size(&mut self) -> io::Result<()> {
        let capabilities = crate::terminal_image::get_capabilities();
        if matches!(
            capabilities.images,
            crate::terminal_image::ImageProtocol::Disabled
        ) {
            return Ok(());
        }
        self.terminal.write(crate::terminal_colors::CELL_SIZE_QUERY)
    }

    /// `tui.ts:527` (`hasOverlayEntries`).
    pub fn has_overlays(&self) -> bool {
        !self.overlays.is_empty()
    }

    /// Hide the most recently shown overlay, whatever its visibility.
    /// `hideOverlay()` with no argument (`tui.ts:739`), which the fullscreen
    /// exit walks to drain the stack (`interactive-mode.ts:634-636`).
    pub fn hide_topmost_overlay(&mut self) {
        if let Some(id) = self.overlays.last().map(OverlayEntry::id) {
            self.hide_overlay(id);
        }
    }

    /// `tui.ts:839` (`hasOverlay`): any *visible* overlay. Where pi gates on
    /// this rather than on `hasOverlayEntries`, a hidden or size-excluded
    /// overlay does not count.
    pub fn has_overlay(&self) -> bool {
        self.overlays
            .iter()
            .any(|entry| self.is_overlay_visible(entry))
    }

    pub fn overlay_count(&self) -> usize {
        self.overlays.len()
    }

    fn find_overlay(&self, id: OverlayId) -> Option<usize> {
        self.overlays
            .iter()
            .position(|entry| entry.generation == id.generation)
    }

    fn reindex_overlays() {
        // Nothing to rewrite: ids are generation based, so removal never
        // invalidates the ids the host still holds.
    }

    // ---------------------------------------------------------------- input

    /// `tui.ts:1044-1120`.
    ///
    /// The host clock stays at zero: only the alt screen's double-click
    /// window and wheel accelerator read it. Hosts that drive those call
    /// [`Self::handle_input_at`].
    pub fn handle_input(&mut self, data: &str) -> bool {
        self.handle_input_at(data, 0)
    }

    /// pi's `this.stopped` guard on both renderers (`tui-alt-screen.ts:1659`).
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// pi's `getShowHardwareCursor()`, `tui-alt-screen.ts:1741`.
    pub(crate) fn hardware_cursor_shown(&self) -> bool {
        self.show_hardware_cursor
    }

    /// [`Self::handle_input`] with the host clock, which the alt screen needs
    /// for its click chain and its wheel accelerator (`tui-alt-screen.ts:700`).
    pub fn handle_input_at(&mut self, data: &str, now_ms: u64) -> bool {
        // pi's flash timers fire on their own; the host clock stands in for
        // them here, so every input is a chance to expire a flash.
        if self.alt_screen_active() {
            self.expire_alt_screen_flashes(now_ms);
        }
        // `handleTerminalInput`, `tui.ts:1045-1050`: the colour-query replies
        // are drained before the input listeners, and the cell-size reply
        // after them (`:1067-1070`).
        if self.consume_color_scheme_report(data) || self.consume_terminal_color_response(data) {
            return true;
        }
        // pi's input listeners run before the focused component
        // (`tui.ts:1052-1065`); the alt screen registers the only one
        // (`tui-alt-screen.ts:280`), and it owns the mouse reports and the
        // `tui.altScreen.*` keys.
        if self.alt_screen_active() && self.handle_alt_viewport_input(data, now_ms) {
            return true;
        }
        if self.consume_cell_size_response(data) {
            return true;
        }
        // A focused overlay that just became invisible loses focus.
        if let Some(FocusTarget::Overlay(id)) = self.focused {
            if let Some(index) = self.find_overlay(id) {
                if !self.is_overlay_visible(&self.overlays[index]) {
                    let pre_focus = self.overlays[index].pre_focus;
                    let target = self
                        .topmost_visible_overlay()
                        .map(|index| FocusTarget::Overlay(self.overlays[index].id()))
                        .or(pre_focus);
                    self.set_focus(target);
                }
            }
        }
        let wants_release = match self.focused {
            Some(FocusTarget::Root(index)) => self
                .root_children()
                .get(index)
                .map(|child| child.wants_key_release()),
            Some(FocusTarget::Overlay(id)) => self
                .find_overlay(id)
                .and_then(|index| self.overlays.get(index))
                .map(|entry| entry.component.wants_key_release()),
            Some(FocusTarget::Id(id)) => self
                .component_mut(id)
                .map(|component| component.wants_key_release()),
            None => None,
        };
        if wants_release != Some(true) && is_key_release(data) {
            return false;
        }
        let mutation = match self.focused {
            Some(FocusTarget::Root(index)) => self
                .root_children_mut()
                .get_mut(index)
                .and_then(|child| child.handle_input(data)),
            Some(FocusTarget::Overlay(id)) => match self.find_overlay(id) {
                Some(index) => self.overlays[index].component.handle_input(data),
                None => None,
            },
            Some(FocusTarget::Id(id)) => self
                .component_mut(id)
                .and_then(|component| component.handle_input(data)),
            None => None,
        };
        // Keyboard input is latency-sensitive. pi avoids the throttled timer
        // path, where even `setTimeout(0)` can take a full 16 ms tick on
        // Windows. `tui.ts:1116-1118`.
        self.request_immediate_render();
        mutation.is_some_and(|mutation| mutation.consume)
    }

    /// Global debug key. `tui.ts:1074-1078`.
    pub fn is_debug_key(data: &str) -> bool {
        matches_key(data, "shift+ctrl+d")
    }

    // ------------------------------------------------------------- renderer

    /// `tui.ts:982-988`.
    pub fn render_now(&mut self, force: bool) {
        if force {
            self.reset_render_state();
        }
        self.render_requested = false;
        // `tui.ts:986` clears the immediate flag too: a frame that went out
        // re-arms the floor for whatever is requested next.
        self.immediate_render = false;
        self.last_render_at = Some(std::time::Instant::now());
        // pi's renderer is one subclass or the other; here the alt screen takes
        // the frame when it is active, exactly as `TuiAltScreen.doRender`
        // replaces `TuiMainScreen.doRender`.
        if self.alt_screen_active() {
            self.alt_screen_do_render();
            return;
        }
        self.do_render();
    }

    /// pi's `MIN_RENDER_INTERVAL_MS` (`tui.ts:507`): the shortest gap between
    /// two frames, which is what turns a stream of deltas into a frame a
    /// terminal can actually show.
    pub const MIN_RENDER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

    /// When a requested frame is due (`scheduleRender`, `tui.ts:1024-1041`):
    /// `max(0, MIN_RENDER_INTERVAL_MS - elapsed)` after the last frame.
    ///
    /// `None` when nothing is pending, so the host does not wake for a frame
    /// that is not owed. pi answers the same question with a `setTimeout`; a
    /// host that has its own clock - a `select!` arm, a test - asks here.
    pub fn render_due_at(&self) -> Option<std::time::Instant> {
        if self.stopped || !self.render_requested {
            return None;
        }
        // `requestImmediateRender` cancels the pending timer, so the request
        // is owed a frame at once (`tui.ts:1008-1017`).
        if self.immediate_render {
            return Some(std::time::Instant::now());
        }
        let now = std::time::Instant::now();
        // pi starts `lastRenderAt` at zero (`tui.ts:506`), so the first request
        // is always due: the elapsed time is the whole run up to it. Nothing
        // has been drawn yet is the same case.
        let Some(last) = self.last_render_at else {
            return Some(now);
        };
        let elapsed = now.checked_duration_since(last).unwrap_or_default();
        Some(now + Self::MIN_RENDER_INTERVAL.saturating_sub(elapsed))
    }

    /// Render a requested frame if it is due, and report whether one went out.
    ///
    /// This is `scheduleRender`'s timer body (`tui.ts:1030-1040`) without the
    /// timer: pi owns its own clock, so it can wait. A host that polls asks
    /// this instead, and gets the same answer - a delta that arrives inside the
    /// interval joins the frame already on its way rather than starting one.
    pub fn render_if_due(&mut self) -> bool {
        let Some(due) = self.render_due_at() else {
            return false;
        };
        if std::time::Instant::now() < due {
            return false;
        }
        self.render_now(false);
        true
    }

    /// Render children and overlays to lines. `tui.ts:394-406` + `compositeOverlays`.
    fn render_lines(&mut self) -> Vec<String> {
        let size = self.terminal.size().unwrap_or(TerminalSize {
            columns: 80,
            rows: 24,
        });
        let width = size.columns as usize;
        let mut lines = Vec::new();
        for child in self.root_children() {
            lines.extend(child.render(width));
        }
        if self.overlays.is_empty() {
            return lines;
        }
        self.composite_overlays(lines, width, size.rows as i64)
    }

    /// `tui-main-screen.ts:247-616`, minus the Kitty image paths.
    // Render loop parity with upstream `doRender`; splitting it would lose the
    // one-to-one mapping with the TS source.
    #[allow(clippy::too_many_lines)]
    fn do_render(&mut self) {
        if self.stopped {
            return;
        }
        let size = self.terminal.size().unwrap_or(TerminalSize {
            columns: 80,
            rows: 24,
        });
        let width = size.columns as i64;
        let height = size.rows as i64;
        let previous = previous_size(self.previous_width, self.previous_height);
        let height_changed = previous.is_some_and(|previous| previous.rows as i64 != height);
        let previous_buffer_length = if self.previous_height > 0 {
            self.previous_viewport_top + self.previous_height
        } else {
            height
        };
        let mut prev_viewport_top = if height_changed {
            (previous_buffer_length - height).max(0)
        } else {
            self.previous_viewport_top
        };
        let mut viewport_top = prev_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;

        let mut new_lines = self.render_lines();
        let cursor_pos = Tui::extract_cursor_position(&mut new_lines, height);
        new_lines = apply_line_resets(new_lines);

        // The frame is decided here, not before the lines are built: only the
        // clear-on-shrink rule looks at the content. `tui-main-screen.ts:330-360`.
        let shrink = redraw_for(RedrawInputs {
            previous_size: previous_size(self.previous_width, self.previous_height),
            size: TerminalSize {
                columns: size.columns,
                rows: size.rows,
            },
            has_previous_lines: !self.previous_lines.is_empty(),
            new_line_count: new_lines.len(),
            max_lines_rendered: self.max_lines_rendered,
            clear_on_shrink: self.clear_on_shrink,
            has_overlays: !self.overlays.is_empty(),
            termux: is_termux_session(),
        });
        match shrink {
            Redraw::First => {
                self.full_render(false, &new_lines, width, height);
                self.position_hardware_cursor(cursor_pos, new_lines.len() as i64);
                return;
            }
            Redraw::Full => {
                self.full_render(true, &new_lines, width, height);
                self.position_hardware_cursor(cursor_pos, new_lines.len() as i64);
                return;
            }
            Redraw::Incremental => {}
        }

        let compute_line_diff = |target_row: i64,
                                 hardware_cursor_row: i64,
                                 prev_viewport_top: i64,
                                 viewport_top: i64| {
            (target_row - viewport_top) - (hardware_cursor_row - prev_viewport_top)
        };

        // Find the changed range. `tui-main-screen.ts:362-388`.
        let max_lines = new_lines.len().max(self.previous_lines.len());
        let mut first_changed: i64 = -1;
        let mut last_changed: i64 = -1;
        for index in 0..max_lines {
            let old_line = self.previous_lines.get(index).map_or("", String::as_str);
            let new_line = new_lines.get(index).map_or("", String::as_str);
            if old_line != new_line {
                if first_changed == -1 {
                    first_changed = index as i64;
                }
                last_changed = index as i64;
            }
        }
        let appended_lines = new_lines.len() > self.previous_lines.len();
        if appended_lines {
            if first_changed == -1 {
                first_changed = self.previous_lines.len() as i64;
            }
            last_changed = new_lines.len() as i64 - 1;
        }

        if first_changed == -1 {
            self.position_hardware_cursor(cursor_pos, new_lines.len() as i64);
            self.previous_viewport_top = prev_viewport_top;
            self.previous_height = height;
            return;
        }

        // Only deletions remain. `tui-main-screen.ts:400-447`.
        if first_changed >= new_lines.len() as i64 {
            if self.previous_lines.len() > new_lines.len() {
                let target_row = (new_lines.len() as i64 - 1).max(0);
                if target_row < prev_viewport_top
                    || self.previous_lines.len() - new_lines.len() > height as usize
                {
                    self.full_render(true, &new_lines, width, height);
                    self.position_hardware_cursor(cursor_pos, new_lines.len() as i64);
                    return;
                }
                let mut output = String::from(BEGIN_SYNC);
                let line_diff = compute_line_diff(
                    target_row,
                    hardware_cursor_row,
                    prev_viewport_top,
                    viewport_top,
                );
                if line_diff > 0 {
                    {
                        use std::fmt::Write as _;
                        let _ = write!(output, "\x1b[{line_diff}B");
                    }
                } else if line_diff < 0 {
                    {
                        use std::fmt::Write as _;
                        let _ = write!(output, "\x1b[{}A", -line_diff);
                    }
                }
                output.push('\r');
                let extra_lines = self.previous_lines.len() - new_lines.len();
                let clear_start_offset = usize::from(!new_lines.is_empty());
                if extra_lines > 0 && clear_start_offset > 0 {
                    {
                        use std::fmt::Write as _;
                        let _ = write!(output, "\x1b[{clear_start_offset}B");
                    }
                }
                for index in 0..extra_lines {
                    output.push_str("\r\x1b[2K");
                    if index < extra_lines - 1 {
                        output.push_str("\x1b[1B");
                    }
                }
                let move_back = extra_lines.saturating_sub(1) + clear_start_offset;
                if move_back > 0 {
                    {
                        use std::fmt::Write as _;
                        let _ = write!(output, "\x1b[{move_back}A");
                    }
                }
                output.push_str(END_SYNC);
                let _ = self.terminal.write(&output);
                self.last_frame = Some(output);
                self.cursor_row = target_row;
                self.hardware_cursor_row = target_row;
            }
            self.position_hardware_cursor(cursor_pos, new_lines.len() as i64);
            self.previous_lines = new_lines.clone();
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = prev_viewport_top;
            return;
        }

        // Differential rendering can only touch what is visible.
        // `tui-main-screen.ts:449-455`.
        if first_changed < prev_viewport_top {
            self.full_render(true, &new_lines, width, height);
            self.position_hardware_cursor(cursor_pos, new_lines.len() as i64);
            return;
        }

        let append_start = appended_lines
            && first_changed == self.previous_lines.len() as i64
            && first_changed > 0;
        let mut output = String::from(BEGIN_SYNC);
        let prev_viewport_bottom = prev_viewport_top + height - 1;
        let move_target_row = if append_start {
            first_changed - 1
        } else {
            first_changed
        };
        if move_target_row > prev_viewport_bottom {
            let current_screen_row = (hardware_cursor_row - prev_viewport_top).clamp(0, height - 1);
            let move_to_bottom = height - 1 - current_screen_row;
            if move_to_bottom > 0 {
                {
                    use std::fmt::Write as _;
                    let _ = write!(output, "\x1b[{move_to_bottom}B");
                }
            }
            let scroll = move_target_row - prev_viewport_bottom;
            output.push_str(&"\r\n".repeat(scroll as usize));
            prev_viewport_top += scroll;
            viewport_top += scroll;
            hardware_cursor_row = move_target_row;
        }

        let line_diff = compute_line_diff(
            move_target_row,
            hardware_cursor_row,
            prev_viewport_top,
            viewport_top,
        );
        if line_diff > 0 {
            {
                use std::fmt::Write as _;
                let _ = write!(output, "\x1b[{line_diff}B");
            }
        } else if line_diff < 0 {
            {
                use std::fmt::Write as _;
                let _ = write!(output, "\x1b[{}A", -line_diff);
            }
        }
        output.push_str(if append_start { "\r\n" } else { "\r" });

        // Only the changed lines are rewritten. `tui-main-screen.ts:487-546`.
        let render_end = last_changed.min(new_lines.len() as i64 - 1);
        let mut render_error: Option<String> = None;
        for index in first_changed..=render_end {
            if index > first_changed {
                output.push_str("\r\n");
            }
            let Some(line) = new_lines.get(index as usize) else {
                continue;
            };
            output.push_str("\x1b[2K");
            if visible_width(line) > width as usize {
                render_error = Some(format!(
                    "Rendered line {index} exceeds terminal width ({} > {}).",
                    visible_width(line),
                    width
                ));
                break;
            }
            output.push_str(line);
        }
        if let Some(error) = render_error {
            // pi throws after stopping the terminal (`tui-main-screen.ts:533`).
            let _ = self.stop(TuiStopOptions::default());
            if let Some(handler) = &self.on_render_error {
                handler(error);
            } else {
                debug_assert!(false, "rendered line exceeded the terminal width");
            }
            return;
        }

        let mut final_cursor_row = render_end;
        if self.previous_lines.len() > new_lines.len() {
            if render_end < new_lines.len() as i64 - 1 {
                let move_down = new_lines.len() as i64 - 1 - render_end;
                {
                    use std::fmt::Write as _;
                    let _ = write!(output, "\x1b[{move_down}B");
                }
                final_cursor_row = new_lines.len() as i64 - 1;
            }
            let extra_lines = self.previous_lines.len() - new_lines.len();
            for _ in 0..extra_lines {
                output.push_str("\r\n\x1b[2K");
            }
            {
                use std::fmt::Write as _;
                let _ = write!(output, "\x1b[{extra_lines}A");
            }
        }
        output.push_str(END_SYNC);
        let _ = self.terminal.write(&output);
        self.last_frame = Some(output);

        self.cursor_row = (new_lines.len() as i64 - 1).max(0);
        self.hardware_cursor_row = final_cursor_row;
        self.max_lines_rendered = self.max_lines_rendered.max(new_lines.len());
        self.previous_viewport_top = prev_viewport_top.max(final_cursor_row - height + 1);
        self.position_hardware_cursor(cursor_pos, new_lines.len() as i64);
        self.previous_lines = new_lines;
        self.previous_width = width;
        self.previous_height = height;
    }

    /// Repaint every line, clearing the screen first when asked.
    /// `tui-main-screen.ts:277-319`.
    fn full_render(&mut self, clear: bool, new_lines: &[String], width: i64, height: i64) {
        self.full_redraw_count += 1;
        let mut output = String::with_capacity(1024);
        output.push_str(BEGIN_SYNC);
        if clear {
            // Clear screen, home, then clear scrollback.
            output.push_str("\x1b[2J\x1b[H\x1b[3J");
        }
        for (index, line) in new_lines.iter().enumerate() {
            if index > 0 {
                output.push_str("\r\n");
            }
            output.push_str(line);
        }
        output.push_str(END_SYNC);
        let _ = self.terminal.write(&output);
        self.last_frame = Some(output);
        self.cursor_row = (new_lines.len() as i64 - 1).max(0);
        self.hardware_cursor_row = self.cursor_row;
        if clear {
            self.max_lines_rendered = new_lines.len();
        } else {
            self.max_lines_rendered = self.max_lines_rendered.max(new_lines.len());
        }
        let buffer_length = height.max(new_lines.len() as i64);
        self.previous_viewport_top = (buffer_length - height).max(0);
        self.previous_lines = new_lines.to_vec();
        self.previous_width = width;
        self.previous_height = height;
    }

    /// `tui-main-screen.ts:623-654`.
    fn position_hardware_cursor(&mut self, cursor_pos: Option<CursorPosition>, total_lines: i64) {
        if cursor_pos.is_none() || total_lines <= 0 {
            return;
        }
        let cursor_pos = cursor_pos.expect("checked");
        let target_row = cursor_pos.row.min(total_lines as usize - 1) as i64;
        let target_col = cursor_pos.col as i64;
        let row_delta = target_row - self.hardware_cursor_row;
        let mut buffer = String::new();
        if row_delta > 0 {
            {
                use std::fmt::Write as _;
                let _ = write!(buffer, "\x1b[{row_delta}B");
            }
        } else if row_delta < 0 {
            {
                use std::fmt::Write as _;
                let _ = write!(buffer, "\x1b[{}A", -row_delta);
            }
        }
        {
            use std::fmt::Write as _;
            let _ = write!(buffer, "\x1b[{}G", target_col + 1);
        }
        if !buffer.is_empty() {
            let _ = self.terminal.write(&buffer);
        }
        self.hardware_cursor_row = target_row;
    }

    /// `tui-main-screen.ts:1442-1460`. Only the visible rows are scanned,
    /// and the marker is stripped from the line that carried it.
    pub(crate) fn extract_cursor_position(
        lines: &mut [String],
        height: i64,
    ) -> Option<CursorPosition> {
        let viewport_top = ((lines.len() as i64) - height).max(0);
        for row in (viewport_top..lines.len() as i64).rev() {
            let line = &lines[row as usize];
            if let Some(marker_index) = line.find(crate::component::CURSOR_MARKER) {
                let before = &line[..marker_index];
                let col = visible_width(before);
                let mut stripped = String::with_capacity(line.len());
                stripped.push_str(before);
                stripped.push_str(&line[marker_index + crate::component::CURSOR_MARKER.len()..]);
                lines[row as usize] = stripped;
                return Some(CursorPosition {
                    row: row as usize,
                    col,
                });
            }
        }
        None
    }

    /// `tui.ts:1204-1302`.
    fn resolve_overlay_layout(
        options: &OverlayOptions,
        overlay_height: usize,
        term_width: usize,
        term_height: i64,
    ) -> (i64, i64, i64, Option<i64>) {
        let margin = options.margin;
        let margin_top = margin.top.unwrap_or(0).max(0);
        let margin_right = margin.right.unwrap_or(0).max(0);
        let margin_bottom = margin.bottom.unwrap_or(0).max(0);
        let margin_left = margin.left.unwrap_or(0).max(0);

        let avail_width = (term_width as i64 - margin_left - margin_right).max(1);
        let avail_height = (term_height - margin_top - margin_bottom).max(1);

        let mut width = options.width.map_or(80.min(avail_width), |value| {
            value.resolve(term_width as i64)
        });
        if let Some(min_width) = options.min_width {
            width = width.max(min_width);
        }
        width = width.clamp(1, avail_width);

        let max_height = options
            .max_height
            .map(|value| value.resolve(term_height).clamp(1, avail_height));
        let effective_height = max_height.map_or(overlay_height as i64, |value| {
            overlay_height.min(value as usize) as i64
        });

        let row = match &options.row {
            Some(SizeValue::Percent(percent)) => {
                let max_row = (avail_height - effective_height).max(0);
                margin_top + (max_row as f64 * percent / 100.0).floor() as i64
            }
            Some(SizeValue::Cells(row)) => *row,
            None => resolve_anchor_row(options.anchor, effective_height, avail_height, margin_top),
        };
        let col = match &options.col {
            Some(SizeValue::Percent(percent)) => {
                let max_col = (avail_width - width).max(0);
                margin_left + (max_col as f64 * percent / 100.0).floor() as i64
            }
            Some(SizeValue::Cells(col)) => *col,
            None => resolve_anchor_col(options.anchor, width, avail_width, margin_left),
        };

        let row = row + options.offset_y;
        let col = col + options.offset_x;
        let row = row.clamp(
            margin_top.min(term_height),
            term_height - margin_bottom - effective_height,
        );
        let col = col.clamp(
            margin_left.min(term_width as i64),
            term_width as i64 - margin_right - width,
        );
        (width, row, col, max_height)
    }

    /// `tui.ts:1339-1411`.
    pub(crate) fn composite_overlays(
        &mut self,
        lines: Vec<String>,
        term_width: usize,
        term_height: i64,
    ) -> Vec<String> {
        if self.overlays.is_empty() {
            return lines;
        }
        let mut result = lines;
        let mut order: Vec<usize> = (0..self.overlays.len()).collect();
        order.sort_by_key(|index| self.overlays[*index].focus_order);

        let mut rendered: Vec<(Vec<String>, i64, i64, usize)> = Vec::new();
        let mut min_lines_needed = result.len() as i64;
        for index in order {
            self.overlays[index].bounds = None;
            let options = match &self.overlays[index] {
                entry if self.is_overlay_visible(entry) => entry.options.clone(),
                _ => continue,
            };
            // Width and max height do not depend on the overlay height.
            let (width, _, _, max_height) =
                Self::resolve_overlay_layout(&options, 0, term_width, term_height);
            let width = width.max(1) as usize;
            let mut overlay_lines = self.overlays[index].component.render(width);
            if let Some(max_height) = max_height {
                overlay_lines.truncate(max_height.max(0) as usize);
            }
            let (_, row, col, _) = Self::resolve_overlay_layout(
                &options,
                overlay_lines.len(),
                term_width,
                term_height,
            );
            self.overlays[index].bounds = Some(OverlayBounds {
                row,
                col,
                width,
                height: overlay_lines.len(),
            });
            min_lines_needed = min_lines_needed.max(row + overlay_lines.len() as i64);
            rendered.push((overlay_lines, row, col, width));
        }

        // Overlays use screen-relative rows, so pad to the terminal height.
        let working_height = (result.len() as i64).max(term_height).max(min_lines_needed);
        while (result.len() as i64) < working_height {
            result.push(String::new());
        }
        let viewport_start = (working_height - term_height).max(0);

        for (overlay_lines, row, col, width) in rendered {
            for (offset, line) in overlay_lines.iter().enumerate() {
                let index = viewport_start + row + offset as i64;
                if index < 0 || index >= result.len() as i64 {
                    continue;
                }
                // Defensive truncation: components should already fit.
                let line = if visible_width(line) > width {
                    crate::ansi::slice_by_column(line, 0, width, true)
                } else {
                    line.clone()
                };
                result[index as usize] = crate::component::composite_tui_line(
                    &result[index as usize],
                    &line,
                    col.max(0) as usize,
                    width,
                    term_width,
                );
            }
        }
        result
    }
}

impl OverlayEntry {
    fn id(&self) -> OverlayId {
        OverlayId {
            index: 0,
            generation: self.generation,
        }
    }
}

fn set_focus_flag(component: &mut dyn Component, focused: bool) {
    component.set_focused(focused);
}

/// `tui.ts:1304-1319`.
fn resolve_anchor_row(
    anchor: OverlayAnchor,
    height: i64,
    avail_height: i64,
    margin_top: i64,
) -> i64 {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => margin_top,
        OverlayAnchor::BottomLeft | OverlayAnchor::BottomCenter | OverlayAnchor::BottomRight => {
            margin_top + avail_height - height
        }
        OverlayAnchor::LeftCenter | OverlayAnchor::Center | OverlayAnchor::RightCenter => {
            margin_top + (avail_height - height) / 2
        }
    }
}

/// `tui.ts:1321-1336`.
fn resolve_anchor_col(
    anchor: OverlayAnchor,
    width: i64,
    avail_width: i64,
    margin_left: i64,
) -> i64 {
    match anchor {
        OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => {
            margin_left
        }
        OverlayAnchor::TopRight | OverlayAnchor::RightCenter | OverlayAnchor::BottomRight => {
            margin_left + avail_width - width
        }
        OverlayAnchor::TopCenter | OverlayAnchor::Center | OverlayAnchor::BottomCenter => {
            margin_left + (avail_width - width) / 2
        }
    }
}

/// `tui.ts:1413-1422`.
pub(crate) fn apply_line_resets(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| format!("{}{SEGMENT_RESET}", normalize_terminal_output(&line)))
        .collect()
}

/// Termux resizes when the software keyboard appears. `tui-main-screen.ts:347`.
fn is_termux_session() -> bool {
    // `Boolean(process.env.TERMUX_VERSION)` (`tui-main-screen.ts:110`): an
    // empty variable is false, so the presence test needs the same rule
    // `keys.rs:480` already uses.
    std::env::var_os("TERMUX_VERSION").is_some_and(|value| !value.is_empty())
}

/// Visible text of a rendered frame, for assertions.
pub fn plain_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{Container, Focusable};
    use crate::terminal::{CaptureTerminal, FrameCapture};

    struct Static(&'static [&'static str]);

    impl Component for Static {
        fn render(&self, _width: usize) -> Vec<String> {
            self.0.iter().map(ToString::to_string).collect()
        }
    }

    struct Keys(Vec<String>);

    impl Component for Keys {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![]
        }

        fn handle_input(&mut self, data: &str) -> Option<crate::component::ComponentMutation> {
            self.0.push(data.to_string());
            Some(crate::component::ComponentMutation { consume: true })
        }
    }

    struct FocusableFlag(bool);

    impl Focusable for FocusableFlag {
        fn set_focused(&mut self, focused: bool) {
            self.0 = focused;
        }
    }

    impl Component for FocusableFlag {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![if self.0 {
                "F".to_string()
            } else {
                "-".to_string()
            }]
        }

        fn set_focused(&mut self, focused: bool) {
            <Self as Focusable>::set_focused(self, focused);
        }
    }

    fn tui(lines: &'static [&'static str], size: (u16, u16)) -> (Tui, FrameCapture) {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: size.0,
            rows: size.1,
        });
        let capture = terminal.capture();
        let mut tui = Tui::new(Box::new(terminal));
        tui.add_child(Box::new(Static(lines)));
        tui.start().unwrap();
        (tui, capture)
    }

    #[test]
    fn first_render_writes_everything_without_clearing() {
        let (mut tui, capture) = tui(&["one", "two"], (10, 5));
        tui.render_now(false);
        let frame = capture.last_frame().expect("a frame");
        assert!(frame.starts_with(BEGIN_SYNC));
        assert!(frame.ends_with(END_SYNC));
        assert!(frame.contains("one"));
        assert!(frame.contains("\r\ntwo"));
        assert!(!frame.contains("\x1b[2J"));
        assert_eq!(tui.full_redraws(), 1);
    }

    #[test]
    fn unchanged_frames_write_nothing() {
        let (mut tui, capture) = tui(&["one"], (10, 5));
        tui.render_now(false);
        capture.clear();
        tui.render_now(false);
        assert_eq!(capture.last_frame(), None);
    }

    #[test]
    fn a_single_changed_line_is_rewritten_in_place() {
        let (mut tui, capture) = tui(&["one"], (10, 5));
        tui.render_now(false);
        capture.clear();
        tui.remove_child(0);
        tui.add_child(Box::new(Static(&["two"])));
        tui.render_now(false);
        let frame = capture.last_frame().expect("a frame");
        assert!(frame.contains("\x1b[2K"));
        assert!(frame.contains("two"));
        assert_eq!(tui.full_redraws(), 1, "a changed line is not a full redraw");
    }

    /// pi's resize handler (`tui.ts:921`): the terminal is asked its size again
    /// and the next frame is the new one. The size the terminal caches is the
    /// whole point — without the re-query the frame keeps laying out at the
    /// dimensions `start` found.
    #[test]
    fn a_resize_re_reads_the_size_and_requests_a_frame() {
        let (mut tui, capture) = tui(&["one"], (10, 5));
        tui.render_now(false);
        assert_eq!(tui.terminal().size().unwrap().columns, 10);

        tui.terminal_mut().set_queried_size(TerminalSize {
            columns: 24,
            rows: 8,
        });
        tui.render_now(false);
        assert!(
            !tui.render_requested(),
            "a rendered frame leaves nothing pending"
        );

        // The user resizes the window.
        tui.handle_resize();
        assert_eq!(
            tui.terminal().size().unwrap(),
            TerminalSize {
                columns: 24,
                rows: 8
            },
            "the cached size is what was re-read"
        );
        assert!(tui.render_requested(), "pi's handler is requestRender");

        capture.clear();
        tui.render_now(false);
        let grid = tui.last_frame_grid();
        assert_eq!(grid.width, 24, "the frame is laid out at the new width");
        assert!(
            grid.row_text(0).starts_with('o'),
            "and it repainted: {grid:?}"
        );
    }

    /// pi's throttle (`scheduleRender`, `tui.ts:1024-1041`): a request that
    /// lands inside the interval since the last frame joins the next one rather
    /// than starting its own.
    ///
    /// Without it a turn that streams a delta per token lays the whole history
    /// out once per token, which is the flicker condition pi pays a timer to
    /// avoid.
    #[test]
    fn a_frame_requested_inside_the_interval_waits_for_the_next_one() {
        const DELTAS: [&[&str]; 3] = [&["on"], &["on e"], &["on eo"]];
        let (mut tui, capture) = tui(&["one"], (10, 5));
        // `start` requested the first frame and nothing has been drawn, so it
        // is due at once (`lastRenderAt` is zero until the first render).
        assert!(tui.render_if_due(), "the first frame is not throttled");
        capture.clear();

        // A stream of deltas, each asking for a frame.
        for delta in DELTAS {
            tui.remove_child(0);
            tui.add_child(Box::new(Static(delta)));
            tui.render_if_due();
        }
        assert_eq!(
            capture.frames().len(),
            0,
            "every request landed inside the interval, so nothing was drawn"
        );

        std::thread::sleep(Tui::MIN_RENDER_INTERVAL);
        assert!(
            tui.render_if_due(),
            "the request after the interval is drawn"
        );
        assert_eq!(
            capture.frames().len(),
            1,
            "one frame, carrying the last state"
        );
        assert!(capture.last_frame().unwrap().contains("on eo"));
    }

    /// A pending frame has a deadline the host can wait on, and none once it
    /// is drawn: pi's `setTimeout` exists for exactly this
    /// (`tui.ts:1029-1041`).
    #[test]
    fn a_pending_frame_has_a_deadline_and_a_drawn_one_does_not() {
        let (mut tui, _capture) = tui(&["one"], (10, 5));
        tui.render_now(false);
        assert_eq!(tui.render_due_at(), None, "nothing is pending");

        tui.request_render();
        let due = tui.render_due_at().expect("a request is owed a frame");
        let wait = due.saturating_duration_since(std::time::Instant::now());
        assert!(
            wait <= Tui::MIN_RENDER_INTERVAL,
            "the wait is at most one interval, never more: {wait:?}"
        );
        tui.render_now(false);
        assert_eq!(tui.render_due_at(), None, "and it is gone once drawn");
    }

    /// pi's immediate path (`requestImmediateRender`, `tui.ts:1001-1017`):
    /// an input-driven request skips the 16 ms floor, and drawing the frame
    /// re-arms it for the next request.
    #[test]
    fn input_preempts_the_throttled_frame() {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 10,
            rows: 3,
        });
        let mut tui = Tui::new(Box::new(terminal));
        let keys = tui.add_child(Box::new(Keys(Vec::new())));
        tui.start().unwrap();
        tui.set_focus(Some(FocusTarget::Root(keys)));
        tui.render_now(false);
        tui.request_render();
        assert!(
            !tui.render_if_due(),
            "a plain request inside the interval still waits"
        );

        // Keyboard input is what takes the immediate path (`tui.ts:1116-1118`).
        assert!(tui.handle_input("a"), "the key reached the component");
        assert!(
            tui.render_due_at()
                .is_some_and(|due| due <= std::time::Instant::now()),
            "and its frame is due at once"
        );
        assert!(tui.render_if_due(), "so the host draws it without waiting");

        tui.request_render();
        assert!(!tui.render_if_due(), "the stamp re-arms the floor");
    }

    /// `requestRender(force)`, `tui.ts:990-995`: a forced request resets the
    /// render state and skips the floor.
    #[test]
    fn a_forced_request_resets_and_renders_at_once() {
        let (mut tui, _capture) = tui(&["one"], (10, 5));
        tui.render_now(false);
        tui.request_render_with_force(false);
        assert!(!tui.render_if_due(), "a plain request still waits");
        tui.request_render_with_force(true);
        assert!(tui.render_if_due(), "a forced one does not");
    }

    #[test]
    fn width_changes_force_a_full_redraw() {
        let (mut tui, capture) = tui(&["one"], (10, 5));
        tui.render_now(false);
        let inner = tui.terminal().size().unwrap();
        // Shrinking the content below the terminal width is not a width
        // change; changing the terminal width is.
        tui.terminal_mut().set_size(TerminalSize {
            columns: inner.columns + 5,
            rows: inner.rows,
        });
        capture.clear();
        tui.render_now(false);
        assert!(capture.last_frame().unwrap().contains("\x1b[2J"));
        assert_eq!(tui.full_redraws(), 2);
    }

    #[test]
    fn deleted_lines_are_cleared() {
        let (mut tui, capture) = tui(&["one", "two"], (10, 5));
        tui.render_now(false);
        capture.clear();
        tui.remove_child(0);
        tui.add_child(Box::new(Static(&["one"])));
        tui.render_now(false);
        assert!(capture.last_frame().is_some());
        assert_eq!(tui.last_lines().len(), 1);
    }

    #[test]
    fn cursor_marker_positions_the_hardware_cursor() {
        struct WithMarker;
        impl Component for WithMarker {
            fn render(&self, _width: usize) -> Vec<String> {
                vec![format!("ab{}", crate::component::CURSOR_MARKER)]
            }
        }
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 10,
            rows: 5,
        });
        let capture = terminal.capture();
        let mut tui = Tui::new(Box::new(terminal));
        tui.add_child(Box::new(WithMarker));
        tui.start().unwrap();
        tui.render_now(false);
        assert!(capture.output().contains("\x1b[3G"));
        assert!(!tui.last_lines()[0].contains(crate::component::CURSOR_MARKER));
    }

    #[test]
    fn lines_are_reset_and_normalized() {
        let (tui, _capture) = tui(&["a\tb"], (10, 5));
        let _ = tui;
        assert_eq!(
            apply_line_resets(vec!["a\tb".to_string()])[0],
            "a   b".to_string() + SEGMENT_RESET
        );
    }

    #[test]
    fn overlays_are_composited_and_recorded_bounds() {
        let (mut tui, capture) = tui(&["base"], (20, 6));
        let id = tui.show_overlay(
            Box::new(Static(&["OV"])),
            Some(OverlayOptions {
                width: Some(SizeValue::Cells(4)),
                ..Default::default()
            }),
        );
        tui.render_now(false);
        let bounds = tui.overlay_bounds(id).expect("bounds");
        assert_eq!(bounds.width, 4);
        let rendered = plain_lines(tui.last_lines());
        assert!(rendered.iter().any(|line| line.contains("OV")));
        assert!(!capture.frames().is_empty());
    }

    #[test]
    fn hidden_overlays_are_not_rendered() {
        let (mut tui, _capture) = tui(&["base"], (20, 6));
        let id = tui.show_overlay(Box::new(Static(&["OV"])), None);
        tui.render_now(false);
        tui.set_overlay_hidden(id, true);
        tui.render_now(false);
        assert!(
            !plain_lines(tui.last_lines())
                .iter()
                .any(|line| line.contains("OV"))
        );
    }

    #[test]
    fn focus_moves_to_a_new_overlay_and_back() {
        let (mut tui, _capture) = tui(&["base"], (20, 6));
        let root = 0;
        tui.set_focus(Some(FocusTarget::Root(root)));
        assert_eq!(tui.focused_component(), Some(0));
        let id = tui.show_overlay(Box::new(Static(&["OV"])), None);
        assert_eq!(tui.focused, Some(FocusTarget::Overlay(id)));
        tui.hide_overlay(id);
        assert_eq!(tui.focused, Some(FocusTarget::Root(root)));
    }

    #[test]
    fn focusable_components_see_the_focus_flag() {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 10,
            rows: 3,
        });
        let mut tui = Tui::new(Box::new(terminal));
        let index = tui.add_child(Box::new(FocusableFlag(false)));
        tui.start().unwrap();
        tui.set_focus(Some(FocusTarget::Root(index)));
        assert_eq!(tui.last_lines().len(), 0);
        tui.render_now(false);
        assert_eq!(tui.child(index).unwrap().render(1)[0], "F");
    }

    #[test]
    fn input_goes_to_the_focused_component() {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 10,
            rows: 3,
        });
        let mut tui = Tui::new(Box::new(terminal));
        let keys = tui.add_child(Box::new(Keys(Vec::new())));
        tui.start().unwrap();
        tui.set_focus(Some(FocusTarget::Root(keys)));
        assert!(tui.handle_input("a"));
        assert!(tui.handle_input("\r"));
        assert!(tui.render_requested(), "input schedules a frame");
    }

    #[test]
    fn key_releases_are_filtered_unless_requested() {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 10,
            rows: 3,
        });
        let mut tui = Tui::new(Box::new(terminal));
        let keys = tui.add_child(Box::new(Keys(Vec::new())));
        tui.start().unwrap();
        tui.set_focus(Some(FocusTarget::Root(keys)));
        assert!(!tui.handle_input("\x1b[97;1:3u"));
    }

    #[test]
    fn stop_moves_past_the_content() {
        let (mut tui, capture) = tui(&["one", "two"], (10, 5));
        tui.render_now(false);
        capture.clear();
        tui.stop(TuiStopOptions::default()).unwrap();
        assert!(capture.output().contains("\r\n"));
    }

    #[test]
    fn container_children_render_in_order() {
        let mut container = Container::new();
        container.add_child(Box::new(Static(&["a"])));
        container.add_child(Box::new(Static(&["b"])));
        assert_eq!(container.render(4), vec!["a", "b"]);
    }

    // ------------------------------------------------ colour query (A.4)

    fn query_tui() -> (Tui, FrameCapture) {
        tui(&[], (20, 5))
    }

    /// The focus-restore state, for assertions. `tui.ts:329-337`.
    fn overlay_focus_restore(tui: &Tui) -> OverlayFocusRestoreState {
        tui.overlay_focus_restore_state()
    }

    fn focused(tui: &Tui) -> Option<FocusTarget> {
        tui.focused_target()
    }

    #[test]
    fn colour_query_writes_pi_s_exact_bytes() {
        let (mut tui, _capture) = query_tui();
        tui.query_terminal_colors(100, None).unwrap();
        assert_eq!(
            tui.last_written().as_deref(),
            Some(terminal_color_query().as_str())
        );
    }

    /// `handleTerminalInput`, `tui.ts:1045-1050` and `:1067-1070`: the colour
    /// replies are drained before the input listeners and the cell-size reply
    /// after them, so neither can be read as a key.
    #[test]
    fn the_input_path_drains_terminal_replies_around_the_viewport_listener() {
        use crate::terminal_image::CellDimensions;
        let (mut tui, _capture) = query_tui();
        tui.query_terminal_colors(100, None).unwrap();
        assert!(
            tui.handle_input_at("\x1b]11;#000000\x07", 0),
            "the OSC reply is a reply, not a key"
        );
        assert!(
            tui.handle_input_at("\x1b[?62;c", 0),
            "and so is the DA1 fence"
        );
        assert!(
            tui.take_terminal_colors().is_some(),
            "the query completed through handle_input"
        );

        crate::terminal_image::set_cell_dimensions(CellDimensions::default());
        assert!(
            tui.handle_input_at("\x1b[6;20;10t", 0),
            "the CSI 6 reply is consumed"
        );
        assert_eq!(
            crate::terminal_image::get_cell_dimensions(),
            CellDimensions {
                width_px: 10,
                height_px: 20
            },
            "`tui.ts:1183-1194` stores the reply as the cell size"
        );
        crate::terminal_image::set_cell_dimensions(CellDimensions::default());
    }

    #[test]
    fn colour_query_completes_on_the_da1_fence() {
        let (mut tui, _capture) = query_tui();
        tui.query_terminal_colors(100, None).unwrap();
        assert!(tui.consume_terminal_response("\x1b]10;#ffffff\x07"));
        assert!(tui.consume_terminal_response("\x1b]11;#000000\x07"));
        // Partial palette: nothing is reported until all sixteen arrive.
        for index in 0..TERMINAL_PALETTE_SIZE - 1 {
            assert!(tui.consume_terminal_response(&format!("\x1b]4;{index};#010101\x07")));
        }
        assert!(tui.take_terminal_colors().is_none());
        assert!(tui.consume_terminal_response("\x1b]4;15;#0f0f0f\x07"));
        assert!(tui.consume_terminal_response("\x1b[?62;c"));
        let colors = tui.take_terminal_colors().expect("completed");
        assert_eq!(
            colors.foreground,
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
        assert_eq!(colors.background, Some(RgbColor { r: 0, g: 0, b: 0 }));
        assert_eq!(
            colors.palette.expect("full palette")[15],
            RgbColor {
                r: 15,
                g: 15,
                b: 15
            }
        );
    }

    #[test]
    fn colour_query_completes_on_the_deadline() {
        let (mut tui, _capture) = query_tui();
        tui.query_terminal_colors(100, None).unwrap();
        tui.consume_terminal_response("\x1b]10;#ffffff\x07");
        assert!(tui.service_terminal_queries(99).is_empty(), "not yet");
        let settled = tui.service_terminal_queries(100);
        let colors = settled.first().expect("deadline");
        assert_eq!(
            colors.foreground,
            Some(RgbColor {
                r: 255,
                g: 255,
                b: 255
            })
        );
        assert!(
            colors.palette.is_none(),
            "a partial palette is not reported"
        );
    }

    #[test]
    fn a_slow_link_delivers_late_replies_to_the_sink() {
        let (mut tui, _capture) = query_tui();
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = slot.clone();
        tui.query_terminal_colors(
            100,
            Some(Box::new(move |colors| {
                *sink.lock().expect("sink") = Some(colors);
            })),
        )
        .unwrap();
        assert_eq!(tui.service_terminal_queries(100).len(), 1);
        assert!(
            slot.lock().expect("slot").is_none(),
            "the deadline only reports the partial result"
        );
        // The rest of the batch lands after the deadline; the sink gets the
        // completed palette, not the truncated one.
        assert!(tui.consume_terminal_response("\x1b]11;#123456\x07"));
        for index in 0..TERMINAL_PALETTE_SIZE {
            tui.consume_terminal_response(&format!("\x1b]4;{index};#010203\x07"));
        }
        assert!(
            tui.consume_terminal_response("\x1b[?62;c"),
            "the DA1 fence completes it"
        );
        let late = (*slot.lock().expect("late reply")).expect("late reply");
        assert_eq!(
            late.background,
            Some(RgbColor {
                r: 0x12,
                g: 0x34,
                b: 0x56
            })
        );
        assert_eq!(
            late.palette.expect("full palette")[0],
            RgbColor { r: 1, g: 2, b: 3 }
        );
    }

    #[test]
    fn an_unrelated_reply_is_not_swallowed() {
        let (mut tui, _capture) = query_tui();
        tui.query_terminal_colors(100, None).unwrap();
        assert!(!tui.consume_terminal_response("x"));
        assert!(!tui.consume_terminal_response("\x1b[A"));
    }

    #[test]
    fn colour_scheme_reports_switch_the_theme() {
        let (mut tui, _capture) = query_tui();
        assert!(tui.consume_terminal_response("\x1b[?997;2n"));
        assert!(tui.consume_terminal_response("\x1b[?997;1n"));
        assert_eq!(
            tui.take_color_scheme_reports(),
            vec![TerminalColorScheme::Light, TerminalColorScheme::Dark]
        );
        assert!(tui.take_color_scheme_reports().is_empty());
    }

    #[test]
    fn a_cell_size_reply_updates_the_image_metrics() {
        let (mut tui, _capture) = query_tui();
        let before = crate::terminal_image::get_cell_dimensions();
        assert!(tui.consume_terminal_response("\x1b[6;20;10t"));
        let after = crate::terminal_image::get_cell_dimensions();
        assert_eq!((after.width_px, after.height_px), (10, 20));
        assert_ne!(before, after);
        crate::terminal_image::set_cell_dimensions(before);
    }

    /// `queryCellSize`, `tui.ts:960-970`: `start` asks for the cell size, and
    /// only when the terminal draws images - the answer is used for nothing
    /// else.
    #[test]
    fn start_asks_for_the_cell_size_only_when_the_terminal_draws_images() {
        use crate::terminal_image::{ImageProtocol, TerminalCapabilities};

        crate::terminal_image::set_capabilities(TerminalCapabilities {
            images: ImageProtocol::Disabled,
            true_color: true,
            hyperlinks: false,
        });
        let plain = CaptureTerminal::with_size(TerminalSize {
            columns: 10,
            rows: 3,
        });
        let plain_bytes = plain.capture();
        let mut tui = Tui::new(Box::new(plain));
        tui.start().unwrap();
        assert!(
            !plain_bytes
                .output()
                .contains(crate::terminal_colors::CELL_SIZE_QUERY)
        );

        crate::terminal_image::set_capabilities(TerminalCapabilities {
            images: ImageProtocol::Kitty,
            true_color: true,
            hyperlinks: false,
        });
        let images = CaptureTerminal::with_size(TerminalSize {
            columns: 10,
            rows: 3,
        });
        let image_bytes = images.capture();
        let mut tui = Tui::new(Box::new(images));
        tui.start().unwrap();
        assert!(
            image_bytes
                .output()
                .contains(crate::terminal_colors::CELL_SIZE_QUERY)
        );
    }

    /// `tui.ts:925-927`, `:950-958`, `:973-975`: the colour-scheme mode is
    /// off until something asks for it, is written once on start and once on
    /// stop, and a repeated toggle writes nothing.
    #[test]
    fn the_colour_scheme_mode_is_written_around_the_session() {
        use crate::terminal_colors::{COLOR_SCHEME_MODE_OFF, COLOR_SCHEME_MODE_ON};

        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 10,
            rows: 3,
        });
        let capture = terminal.capture();
        let mut tui = Tui::new(Box::new(terminal));
        tui.add_child(Box::new(Static(&["one"])));
        assert!(!tui.color_scheme_notifications(), "off by default");
        tui.start().unwrap();
        assert!(
            !capture.output().contains(COLOR_SCHEME_MODE_ON),
            "start stays silent"
        );

        tui.set_color_scheme_notifications(true);
        assert!(tui.color_scheme_notifications());
        tui.set_color_scheme_notifications(true);
        tui.stop(TuiStopOptions::default()).unwrap();
        let written = capture.output();
        assert_eq!(
            written.matches(COLOR_SCHEME_MODE_ON).count(),
            1,
            "written once: {written:?}"
        );
        assert_eq!(
            written.matches(COLOR_SCHEME_MODE_OFF).count(),
            1,
            "dropped once: {written:?}"
        );
        assert!(
            written.find(COLOR_SCHEME_MODE_ON) < written.find(COLOR_SCHEME_MODE_OFF),
            "the mode is set after the cursor is hidden and dropped before the trailing newline"
        );
    }

    /// `tui.ts:1189-1191`: a zero-sized report is consumed and changes
    /// nothing, rather than reaching a component as text.
    #[test]
    fn a_zero_sized_cell_report_is_eaten_and_changes_nothing() {
        let (mut tui, _capture) = query_tui();
        let before = crate::terminal_image::get_cell_dimensions();
        assert!(
            tui.consume_terminal_response("\x1b[6;0;10t"),
            "pi consumes it"
        );
        assert!(
            tui.consume_terminal_response("\x1b[6;20;0t"),
            "either way round"
        );
        // A dimension past a `u32` is the same case: pi's `parseInt` keeps the
        // double, this port cannot, so the report is eaten and dropped.
        assert!(tui.consume_terminal_response("\x1b[6;4294967296;10t"));
        assert_eq!(crate::terminal_image::get_cell_dimensions(), before);
        // Not a report at all: pi passes it on.
        assert!(!tui.consume_terminal_response("\x1b[6;-1;10t"));
    }

    // --------------------------------------- overlay focus restore (A.5)

    #[test]
    fn a_nested_overlay_blocks_focus_leaving_it_and_gets_it_back() {
        let (mut tui, _capture) = query_tui();
        tui.add_child(Box::new(Static(&["root"])));
        let outer = tui.show_overlay(Box::new(Static(&["outer"])), None);
        assert_eq!(focused(&tui), Some(FocusTarget::Overlay(outer)));
        assert_eq!(
            overlay_focus_restore(&tui),
            OverlayFocusRestoreState::Eligible { overlay: outer }
        );

        // Focusing the root from inside the overlay is blocked: the overlay
        // keeps the claim so a closing submenu can take focus back.
        tui.set_focus(Some(FocusTarget::Root(0)));
        assert_eq!(
            overlay_focus_restore(&tui),
            OverlayFocusRestoreState::Blocked {
                overlay: outer,
                blocked_by: component_id(tui.child(0).expect("root")),
                resume: OverlayFocusResume::RestoreOverlay,
            }
        );

        // Clearing focus then resolves the block back to the overlay.
        tui.set_focus_with_policy(None, OverlayFocusRestorePolicy::Clear);
        assert_eq!(focused(&tui), Some(FocusTarget::Overlay(outer)));
    }

    #[test]
    fn hiding_the_blocking_overlay_drops_the_restore_state() {
        let (mut tui, _capture) = query_tui();
        tui.add_child(Box::new(Static(&["root"])));
        let outer = tui.show_overlay(Box::new(Static(&["outer"])), None);
        tui.set_focus(Some(FocusTarget::Root(0)));
        assert!(matches!(
            overlay_focus_restore(&tui),
            OverlayFocusRestoreState::Blocked { .. }
        ));
        tui.hide_overlay(outer);
        assert_eq!(
            overlay_focus_restore(&tui),
            OverlayFocusRestoreState::Inactive
        );
    }

    #[test]
    fn focusing_outside_then_back_does_not_leave_a_stale_block() {
        let (mut tui, _capture) = query_tui();
        tui.add_child(Box::new(Static(&["root"])));
        let outer = tui.show_overlay(Box::new(Static(&["outer"])), None);
        tui.set_focus(Some(FocusTarget::Root(0)));
        assert!(matches!(
            overlay_focus_restore(&tui),
            OverlayFocusRestoreState::Blocked { .. }
        ));
        // Focusing the overlay again makes it eligible for a fresh cycle.
        tui.set_focus(Some(FocusTarget::Overlay(outer)));
        assert_eq!(
            overlay_focus_restore(&tui),
            OverlayFocusRestoreState::Eligible { overlay: outer }
        );
    }

    #[test]
    fn a_non_capturing_overlay_never_takes_focus() {
        let (mut tui, _capture) = query_tui();
        let options = OverlayOptions {
            non_capturing: true,
            ..OverlayOptions::default()
        };
        let id = tui.show_overlay(Box::new(Static(&["passive"])), Some(options));
        assert_eq!(focused(&tui), None);
        tui.hide_overlay(id);
    }

    #[test]
    fn debug_key_is_recognised() {
        assert!(Tui::is_debug_key("\x1b[68;6u"));
        assert!(!Tui::is_debug_key("d"));
    }

    // ------------------------------------- focus by identity (fullscreen)

    /// `FocusTarget::Id`: a component nested inside a root child — the
    /// fullscreen dock's editor — receives keys when focused by identity.
    /// Upstream focuses the same component object across renderer switches;
    /// the port names it by identity instead.
    #[test]
    fn focus_by_identity_reaches_a_nested_component() {
        let (mut tui, _capture) = tui(&["one"], (10, 5));
        let keys = Box::new(Keys(Vec::new()));
        let keys_id = component_id(keys.as_ref());
        let mut container = Container::new();
        container.add_child(keys);
        tui.add_child(Box::new(container));
        tui.start().unwrap();
        tui.set_focus(Some(FocusTarget::Id(keys_id)));
        assert_eq!(tui.focused_id_value(), keys_id);
        assert!(
            tui.handle_input("x"),
            "the key reached the nested component"
        );
    }

    /// [`Tui::set_focus_by_id`]: the shallow scans miss a nested component;
    /// the deep walk focuses it by identity.
    #[test]
    fn set_focus_by_id_finds_a_nested_component() {
        let (mut tui, _capture) = tui(&["one"], (10, 5));
        let keys = Box::new(Keys(Vec::new()));
        let keys_id = component_id(keys.as_ref());
        let mut container = Container::new();
        container.add_child(keys);
        tui.add_child(Box::new(container));
        tui.start().unwrap();
        assert!(
            tui.set_focus_by_id(keys_id),
            "the nested component is mounted"
        );
        assert_eq!(tui.focused_id_value(), keys_id);
        // Delivery follows: the identity target resolves through the same
        // walk.
        assert!(tui.handle_input("y"));
    }
}
