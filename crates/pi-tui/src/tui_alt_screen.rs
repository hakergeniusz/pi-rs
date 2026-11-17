//! The alt-screen renderer.
//!
//! Ground truth: `packages/tui/src/tui-alt-screen.ts` (1755 lines). pi models
//! this as `TuiAltScreen extends TuiBase`, so here it is an inherent impl on
//! [`Tui`] plus the state this module owns; the base `Tui` supplies the
//! overlay stack, the focus owner and the terminal through a narrow
//! `pub(crate)` seam (`tui.rs:838-940`).
//!
//! Two pi features have no counterpart in this crate and are recorded rather
//! than approximated:
//!
//! - **Flashes** (`AltScreenFlashContainer`, `:1657-1680`) expire on a wall
//!   clock, and `pi-tui` owns no timer. Nothing else here depends on them.
//! - **Clipboard** (`copyActiveSelectionToClipboard`, `:309-314`) needs an
//!   async clipboard backend this crate does not own. The selection text is
//!   readable through [`Tui::alt_screen_active_selection_text`] and the
//!   copy-on-select request through [`Tui::take_alt_screen_copy_request`],
//!   which is the whole of what a host has to supply.
//!
//! Text selection's *rendering* (`:1129-1628`) is **not** ported here; it needs
//! the rendered-line cache and a word segmenter driven from `utils.ts`, and it
//! is recorded as pending in `01-recon.md` rather than half-landed. Everything
//! that decides *routing* is ported, in pi's order — including the
//! press/release bookkeeping that decides where a click is (see
//! `handle_selection_mouse_event`), which is why a click on the editor moves
//! the cursor even though the editor ignores the press that precedes it.

use crate::alt_screen_search::{
    AltScreenSearchComponent, AltScreenSearchComponentOptions, AltScreenSearchIndex,
    SEARCH_DIRECTION_PREVIOUS, SearchNavigationDirection,
};
use crate::component::DynComponent;
use crate::components::scroll_view::RenderRequest;
use crate::layout::{
    LayoutFrame, ScrollbarGeometry, apply_scroll_updates, get_layout_boxes_at, get_scroll_view_box,
    get_scroll_views_at, get_scrollbar_geometry, render_layout_frame,
};
use crate::mouse::{
    ComponentClickTracker, ComponentId, MouseDispatchResult, MouseDispatchTarget, MouseEvent,
    MouseEventKind, SgrMouseEvent, WheelEvent, event_kind_for, is_osc133_prompt_start,
    retarget_mouse_event, strip_osc133_zone_prefix,
};
use crate::tui::{OverlayBounds, OverlayId, OverlayMargin, SizeValue, Tui, TuiStopOptions};
use crate::wheel_scroll::{WheelScrollAccelerator, WheelScrollLines};
/// `tui-alt-screen.ts:50-67`.
pub const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
/// Leave the alternate screen; pairs with [`ENTER_ALT_SCREEN`].
pub const EXIT_ALT_SCREEN: &str = "\x1b[?1049l";
/// Turn autowrap off (DECRST 7): the painter writes fixed-width lines.
pub const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
/// Turn autowrap back on before the terminal is handed back.
pub const ENABLE_AUTOWRAP: &str = "\x1b[?7h";
/// Button-motion tracking: enough for clicks, the wheel, selection and
/// scrollbar drags without forwarding every pointer move.
pub const ENABLE_BUTTON_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h";
/// All-motion tracking, used when no multiplexer is detected.
pub const ENABLE_ALL_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h";
/// Disable everything the enable sequences can set, in reverse order.
pub const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
/// The terminal's focus-gained report, consumed by [`Tui::alt_screen_handle_focus`].
pub const FOCUS_IN: &str = "\x1b[I";
/// The terminal's focus-lost report; handling it also releases every gesture.
pub const FOCUS_OUT: &str = "\x1b[O";
/// Begin a synchronized update (DECSET 2026); the enclosed writes land atomically.
pub const BEGIN_SYNCHRONIZED_OUTPUT: &str = "\x1b[?2026h";
/// End the synchronized update.
pub const END_SYNCHRONIZED_OUTPUT: &str = "\x1b[?2026l";

/// `PAGE_SCROLL_OVERLAP`, `tui-alt-screen.ts:69`.
pub const PAGE_SCROLL_OVERLAP: usize = 4;
/// `ALT_WHEEL_SCROLL_MULTIPLIER`, `tui-alt-screen.ts:70`.
pub const ALT_WHEEL_SCROLL_MULTIPLIER: i64 = 5;
/// `MAX_CACHED_OFFSCREEN_KITTY_IMAGES`, `tui-alt-screen.ts:77`.
const MAX_CACHED_OFFSCREEN_KITTY_IMAGES: usize = 16;
/// `MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES`, `:78`.
const MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES: usize = 32 * 1024 * 1024;
/// `MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES`, `:79`.
const MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES: u64 = 64 * 1024 * 1024;
/// `DOUBLE_CLICK_INTERVAL_MS`, `tui-alt-screen.ts:80`.
const DOUBLE_CLICK_INTERVAL_MS: u64 = 500;

/// Options for the alt-screen mode. `TuiAltScreenOptions`, `:166-178`.
pub struct AltScreenOptions {
    /// Track the mouse. `mouse ?? true`.
    pub mouse: bool,
    /// The wheel accelerator's line count. `wheelScrollLines ?? 1`.
    pub wheel_scroll_lines: Option<WheelScrollLines>,
    /// The label the scroll-to-end indicator draws. `scrollToEndIndicator`.
    pub scroll_to_end_indicator: Option<Box<dyn Fn() -> String + Send>>,
    /// Open an OSC 8 hyperlink activated with a primary-button click.
    /// `openUrl`, `:190`.
    pub open_url: Option<Box<dyn Fn(String) + Send>>,
    /// Right-click paste. Windows terminals only (`:1010-1025`).
    pub on_right_click_paste: Option<Box<dyn Fn() + Send>>,
    /// Copy a completed selection. `copyOnSelect ?? true`.
    pub copy_on_select: bool,
    /// Style a non-current transcript search match. `searchMatchStyle`,
    /// `:173`. pi passes `theme.underline(theme.bg(...))` from the theme
    /// controller; the identity here is what an unstyled caller gets.
    pub search_match_style: SearchTextStyle,
    /// Style the current transcript search match. `searchCurrentMatchStyle`,
    /// `:175`.
    pub search_current_match_style: SearchTextStyle,
    /// Style a transcript search navigation button. `searchNavigationButtonStyle`,
    /// `:177`, forwarded to the search overlay component.
    pub search_navigation_button_style: crate::alt_screen_search::SearchButtonStyle,
}

/// `SearchHighlightRange`, `tui-alt-screen.ts:161-165`: one run of a match on
/// one row, and whether it is the selected match.
struct SearchHighlightRange {
    start_col: usize,
    end_col: usize,
    current: bool,
}

/// `(text) -> styled text`. `searchMatchStyle`, `tui-alt-screen.ts:173-175`.
pub type SearchTextStyle = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>;

fn identity_style() -> SearchTextStyle {
    std::sync::Arc::new(|text: &str| text.to_string())
}

impl Default for AltScreenOptions {
    fn default() -> Self {
        Self {
            mouse: true,
            wheel_scroll_lines: None,
            scroll_to_end_indicator: None,
            open_url: None,
            on_right_click_paste: None,
            copy_on_select: true,
            search_match_style: identity_style(),
            search_current_match_style: identity_style(),
            search_navigation_button_style: std::sync::Arc::new(|text: &str, _hovered: bool| {
                text.to_string()
            }),
        }
    }
}

/// The environment pi consults before choosing a mouse sequence.
/// `tui-alt-screen.ts:389-398`.
#[derive(Debug, Clone, Default)]
pub struct MultiplexerEnv {
    pub tmux: bool,
    pub zellij: bool,
    pub sty: bool,
    /// The lower-cased `TERM`, when set.
    pub term: Option<String>,
}

impl MultiplexerEnv {
    /// Read the same variables pi reads (`process.env`, `:389-393`).
    pub fn from_process() -> Self {
        Self {
            tmux: std::env::var_os("TMUX").is_some(),
            zellij: std::env::var_os("ZELLIJ").is_some(),
            sty: std::env::var_os("STY").is_some(),
            term: std::env::var("TERM").ok().map(|term| term.to_lowercase()),
        }
    }

    /// Which enable sequence pi writes. `:389-398`.
    pub fn mouse_sequence(&self) -> &'static str {
        let term = self.term.as_deref().unwrap_or("");
        if self.tmux
            || self.zellij
            || self.sty
            || term.starts_with("tmux")
            || term.starts_with("screen")
        {
            ENABLE_BUTTON_MOTION_MOUSE
        } else {
            ENABLE_ALL_MOTION_MOUSE
        }
    }
}

/// Which search action the next refresh performs.
/// `SearchSelectionMode`, `:158-160`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSelectionMode {
    /// A fresh query: pick the first match at or after the anchor row.
    Query,
    Next,
    Previous,
    /// Keep the current selection across a content change.
    Retain,
}

/// A point in selection space. `SelectionPoint`, `:93-99`.
///
/// `row` is relative to the scroll view's content when `scroll_view` is set,
/// and to the screen otherwise. `boundary` — pi's flag for a point between two
/// terminal cells rather than on one — is not yet produced by anything here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionPoint {
    row: i64,
    col: i64,
    scroll_view: Option<ComponentId>,
    /// `boundary` (`:97-98`): the point lies *between* cells rather than on
    /// one, which is how the end of a word or line selection is expressed.
    boundary: bool,
}

/// How a drag grows the selection. `SelectionGranularity`, `:106`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionGranularity {
    Character,
    Word,
    Line,
}

/// A word or line span. `SelectionRange`, `:101-104`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionRange {
    start: SelectionPoint,
    end: SelectionPoint,
}

/// The last click that could extend a word or line selection. `ClickTarget`,
/// `:108-115`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClickTarget {
    count: u32,
    row: i64,
    scroll_view: Option<ComponentId>,
    word_start: i64,
    word_end: i64,
}

/// A scrollbar press or drag. `ScrollbarDrag`, `:136-139`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollbarDrag {
    scroll_view: ComponentId,
    grab_offset: i64,
}

/// The area the scroll-to-end label occupies, for its own hit test.
/// `scrollToEndIndicatorRect`, `:141-145`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollToEndIndicatorRect {
    pub row: i64,
    pub column: i64,
    pub width: i64,
}

/// The live search session. `ActiveSearch`, `:150-160`.
struct ActiveSearch {
    overlay: OverlayId,
    index: AltScreenSearchIndex,
    query: String,
    /// `search.matches`, `tui-alt-screen.ts:154`: the hits themselves, which
    /// the highlight painter walks row by row. `selected_index` counts into
    /// this vector.
    matches: Vec<crate::alt_screen_search::AltScreenSearchMatch>,
    selected_index: i64,
    selected_key: Option<String>,
    anchor_row: usize,
    selection_mode: SearchSelectionMode,
}

/// Everything the alt-screen mode owns.
// The booleans mirror upstream TS state flags (`tui-alt-screen.ts`); the port
// keeps them one-to-one.
#[allow(clippy::struct_excessive_bools)]
pub struct AltScreenState {
    /// `mouseCapture`, `tui-alt-screen.ts:881-947`.
    mouse_capture: Option<MouseDispatchTarget>,
    /// `mousePressTarget`.
    mouse_press_target: Option<MouseDispatchTarget>,
    /// `mousePressPoint`.
    mouse_press_point: Option<(i64, i64)>,
    /// `mousePressMoved`.
    mouse_press_moved: bool,
    /// `lastComponentClick`: keyed on component and exact cell, 500 ms.
    last_component_click: ComponentClickTracker,
    /// `selectionPressActive`, `:225`: a left button went down on the screen
    /// rather than on a component that claimed it.
    selection_press_active: bool,
    /// `selectionAnchor`.
    selection_anchor: Option<SelectionPoint>,
    /// `selectionFocus`.
    selection_focus: Option<SelectionPoint>,
    /// `selectionDragged`, `:231`.
    selection_dragged: bool,
    /// `lastClick`, `:221`.
    last_click: Option<(ClickTarget, u64)>,
    /// `selectionGranularity`, `:219`.
    selection_granularity: SelectionGranularity,
    /// `selectionInitialRange`, `:220`: the word or line the press landed on,
    /// which the drag grows from in either direction.
    selection_initial_range: Option<SelectionRange>,
    /// `selectionDragPointer`, `:222`.
    selection_drag_pointer: Option<(i64, i64)>,
    /// `selectionAutoScrollDirection`, `:223`.
    selection_auto_scroll_direction: i8,
    /// `pressedUrl`, `:230`: the OSC 8 link under the press, if any.
    pressed_url: Option<String>,
    /// The last painted frame, as written. `previousScreen`, `:316`.
    previous_screen: Vec<String>,
    wheel_scroll: WheelScrollAccelerator,
    /// `scrollbarHover`.
    scrollbar_hover: Option<ComponentId>,
    /// `scrollbarDrag`.
    scrollbar_drag: Option<ScrollbarDrag>,
    /// `scrollToEndIndicatorRect`, recomputed on every composite.
    scroll_to_end_indicator_rect: Option<ScrollToEndIndicatorRect>,
    active_search: Option<ActiveSearch>,
    /// `altScreenActive`, so a double stop cannot write the exit twice.
    alt_screen_active: bool,
    options_mouse: bool,
    scroll_to_end_indicator: Option<Box<dyn Fn() -> String + Send>>,
    /// `openUrl`, `:190`: open the OSC 8 link under a click.
    open_url: Option<Box<dyn Fn(String) + Send>>,
    on_right_click_paste: Option<Box<dyn Fn() + Send>>,
    copy_on_select: bool,
    /// Set when a completed selection asked to be copied. This crate owns no
    /// clipboard, so the host drains it.
    copy_requested: bool,
    /// The active selection's text. `getActiveSelectionText`, `:1431`.
    selection_text: Option<String>,
    /// The document root, when the host mounted one. `layoutRoot`, `:320-326`.
    root: Option<ComponentId>,
    /// `implicitScrollView`, `tui-alt-screen.ts:211, :268`: the alt screen
    /// always owns a primary, follow-end scroll view whose document is the
    /// host's own root children. It exists whenever the alt screen has been
    /// entered, and it is the layout root whenever [`Self::root`] is unset.
    pub(crate) implicit_root: Option<Box<crate::components::scroll_view::ScrollView>>,
    /// `flashes`, `tui-alt-screen.ts:212`: transient messages the renderer
    /// composites over the frame. Not a child component, because nothing else
    /// reads it and pi keeps it out of the tree too.
    pub(crate) flashes: crate::components::alt_screen_flash::AltScreenFlashContainer,
    /// `searchMatchStyle` / `searchCurrentMatchStyle`, `:173-175`: the painter's
    /// two hooks, kept because the theme is applied after the alt screen is
    /// built.
    pub(crate) search_match_style: SearchTextStyle,
    pub(crate) search_current_match_style: SearchTextStyle,
    /// Forwarded to the search overlay component when it is built.
    pub(crate) search_navigation_button_style: crate::alt_screen_search::SearchButtonStyle,
    /// `previousScreenWidth` / `previousScreenHeight`, `:206-207`: a change in
    /// either forces the full redraw that `:1694-1695` decides on.
    pub(crate) previous_screen_width: usize,
    pub(crate) previous_screen_height: usize,
    /// `imageProtocol`, `:347`: the capabilities read at enter, kept so the
    /// painter knows whether the frame carries Kitty or iTerm2 images.
    pub(crate) image_protocol: crate::terminal_image::ImageProtocol,
    /// `uploadedKittyImages`, `:421-426`: images transmitted at least once and
    /// still believed to be on screen, which the painter replaces with a plain
    /// line when the same transmission generation comes round again.
    pub(crate) uploaded_kitty_images: Vec<(u32, CachedKittyImage)>,
    /// `savedCapabilities`, `:352`: the capabilities to restore when the alt
    /// screen stops, for the iTerm2 case where entering disables them.
    pub(crate) saved_capabilities: Option<crate::terminal_image::TerminalCapabilities>,
}

/// `CachedKittyImage`, `tui-alt-screen.ts:107-111`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CachedKittyImage {
    pub(crate) transmission_generation: u64,
    pub(crate) transmission_bytes: usize,
    pub(crate) estimated_decoded_bytes: u64,
}

impl Default for AltScreenState {
    fn default() -> Self {
        Self::new()
    }
}

impl AltScreenState {
    /// The host's root children, which live in the implicit document once the
    /// alt screen has been entered. `implicitDocument`, `:261-267`.
    pub(crate) fn implicit_document(&self) -> Option<&[Box<DynComponent>]> {
        self.implicit_root
            .as_ref()
            .and_then(|root| crate::component::implicit_document_of(root.child()))
            .map(crate::component::ImplicitDocument::children)
    }

    pub(crate) fn implicit_document_mut(&mut self) -> Option<&mut Vec<Box<DynComponent>>> {
        self.implicit_root
            .as_mut()
            .and_then(|root| crate::component::implicit_document_mut(root.child_mut()))
    }
    /// A fresh state: no gestures, no selection, no screen history, default options.
    pub fn new() -> Self {
        Self {
            mouse_capture: None,
            mouse_press_target: None,
            mouse_press_point: None,
            mouse_press_moved: false,
            last_component_click: ComponentClickTracker::new(),
            selection_press_active: false,
            selection_anchor: None,
            selection_focus: None,
            selection_dragged: false,
            last_click: None,
            selection_granularity: SelectionGranularity::Character,
            selection_initial_range: None,
            selection_drag_pointer: None,
            selection_auto_scroll_direction: 0,
            pressed_url: None,
            previous_screen: Vec::new(),
            wheel_scroll: WheelScrollAccelerator::default(),
            scrollbar_hover: None,
            scrollbar_drag: None,
            scroll_to_end_indicator_rect: None,
            active_search: None,
            alt_screen_active: false,
            options_mouse: true,
            scroll_to_end_indicator: None,
            open_url: None,
            on_right_click_paste: None,
            copy_on_select: true,
            copy_requested: false,
            selection_text: None,
            root: None,
            implicit_root: None,
            flashes: crate::components::alt_screen_flash::AltScreenFlashContainer::new(),
            previous_screen_width: 0,
            previous_screen_height: 0,
            image_protocol: crate::terminal_image::ImageProtocol::Disabled,
            uploaded_kitty_images: Vec::new(),
            saved_capabilities: None,
            search_match_style: identity_style(),
            search_current_match_style: identity_style(),
            search_navigation_button_style: std::sync::Arc::new(|text: &str, _hovered: bool| {
                text.to_string()
            }),
        }
    }
}

/// The outcome of [`Tui::dispatch_mouse_to_overlay`]. `tui.ts:861-882`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AltOverlayHit {
    /// An overlay's rectangle is under the pointer, whether or not it consumed.
    pub hit: bool,
    /// Which overlay was hit.
    pub overlay: Option<OverlayId>,
    /// What it returned, if anything.
    pub result: Option<MouseDispatchResult>,
}

/// What the alt screen did with a mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseOutcome {
    Consumed,
    /// Nothing claimed it; the host may fall through to its own handling.
    Ignored,
}

impl Tui {
    fn alt_state(&mut self) -> &mut AltScreenState {
        self.alt_screen.get_or_insert_with(AltScreenState::new)
    }

    pub(crate) fn alt(&self) -> Option<&AltScreenState> {
        self.alt_screen.as_ref()
    }

    /// Whether the alt-screen mode is running.
    pub fn alt_screen_active(&self) -> bool {
        self.alt().is_some_and(|alt| alt.alt_screen_active)
    }

    /// Copy a completed selection when it finishes. `setCopyOnSelect`,
    /// `tui-alt-screen.ts:237-239`; pi re-applies it from the settings
    /// selector (`applyRuntimeSettings`, `interactive-mode.ts:1674-1677`).
    pub fn set_copy_on_select(&mut self, enabled: bool) {
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.copy_on_select = enabled;
        }
    }

    /// Lines per wheel event. `setWheelScrollLines`, `tui-alt-screen.ts:233-235`;
    /// resets the accelerator so a mid-gesture change starts a new gesture.
    pub fn set_wheel_scroll_lines(&mut self, lines: WheelScrollLines) {
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.wheel_scroll.set_lines(lines);
        }
    }

    // ---------------------------------------------------------- enter/exit

    /// `beforeTerminalStart`, `tui-alt-screen.ts:337-366`.
    ///
    /// Resets every piece of gesture state — a restarted session must not
    /// inherit a half-finished press — then writes the enter sequence. The
    /// multiplexer check and the autowrap/cursor sequences are byte-for-byte
    /// pi's; [`MultiplexerEnv`] is injected so no test reads the environment.
    // Public API: the environment is taken by value, matching the injected
    // fixture pattern callers already use.
    #[allow(clippy::needless_pass_by_value)]
    pub fn enter_alt_screen_with(
        &mut self,
        options: AltScreenOptions,
        env: MultiplexerEnv,
    ) -> std::io::Result<()> {
        let AltScreenOptions {
            mouse,
            wheel_scroll_lines,
            scroll_to_end_indicator,
            open_url,
            on_right_click_paste,
            copy_on_select,
            search_match_style,
            search_current_match_style,
            search_navigation_button_style,
        } = options;
        self.close_search();
        self.stop_scrollbar_hover();
        self.stop_scrollbar_drag();
        self.stop_selection_auto_scroll();
        {
            let alt = self.alt_state();
            // `flashes.dispose()`, `:344`: a restarted session starts with an
            // empty stack.
            alt.flashes.dispose();
            alt.alt_screen_active = true;
            alt.scroll_to_end_indicator = scroll_to_end_indicator;
            alt.open_url = open_url;
            alt.on_right_click_paste = on_right_click_paste;
            alt.copy_on_select = copy_on_select;
            alt.search_match_style = search_match_style;
            alt.search_current_match_style = search_current_match_style;
            alt.search_navigation_button_style = search_navigation_button_style;
            alt.options_mouse = mouse;
            alt.selection_text = None;
            alt.copy_requested = false;
            alt.active_search = None;
            alt.selection_press_active = false;
            alt.selection_anchor = None;
            alt.selection_focus = None;
            alt.selection_granularity = SelectionGranularity::Character;
            alt.selection_initial_range = None;
            alt.last_click = None;
            alt.pressed_url = None;
            alt.selection_dragged = false;
            alt.previous_screen = Vec::new();
            alt.scroll_to_end_indicator_rect = None;
            alt.previous_screen_width = 0;
            alt.previous_screen_height = 0;
            // `imageProtocol` / `savedCapabilities`, `:347-355`: an iTerm2
            // terminal has its capabilities turned off for the alt screen and
            // back on the way out, so nothing else draws inline images into
            // the same screen.
            let capabilities = crate::terminal_image::get_capabilities();
            alt.image_protocol = capabilities.images;
            alt.uploaded_kitty_images.clear();
            if capabilities.images == crate::terminal_image::ImageProtocol::Iterm2 {
                alt.saved_capabilities = Some(capabilities);
                crate::terminal_image::set_capabilities(
                    crate::terminal_image::TerminalCapabilities {
                        images: crate::terminal_image::ImageProtocol::Disabled,
                        ..capabilities
                    },
                );
            }
            if let Some(lines) = wheel_scroll_lines {
                alt.wheel_scroll.set_lines(lines);
            }
        }
        self.ensure_implicit_root();
        self.clear_alt_mouse_gesture();
        let sequence = format!(
            "{ENTER_ALT_SCREEN}{DISABLE_AUTOWRAP}{}\x1b[2J\x1b[H\x1b[?25l",
            if mouse { env.mouse_sequence() } else { "" }
        );
        self.write_terminal(&sequence);
        self.reset_render_state();
        self.request_render();
        Ok(())
    }

    /// [`Tui::enter_alt_screen_with`] reading the live environment.
    pub fn enter_alt_screen(&mut self, options: AltScreenOptions) -> std::io::Result<()> {
        self.enter_alt_screen_with(options, MultiplexerEnv::from_process())
    }

    /// `switchTuiMode`'s renderer swap for the port's single-`Tui` shape
    /// (`interactive-mode.ts:661`): leave the alt screen for the main screen
    /// *without* stopping the terminal. The alt screen's before/after stop
    /// hooks run with `options` (`preserveScreen` decides between handing the
    /// terminal back untouched and repainting the transcript), the alt state
    /// is dropped so child operations route flat again, and any root children
    /// the host did not unmount first come back for re-mounting.
    pub fn exit_alt_screen(
        &mut self,
        options: TuiStopOptions,
    ) -> Vec<Box<dyn crate::component::Component>> {
        if self.alt_screen_active() {
            self.before_alt_screen_stop();
            self.after_alt_screen_stop(options);
        }
        self.alt_root = None;
        let Some(mut alt) = self.alt_screen.take() else {
            return self.take_root_children();
        };
        // The implicit scroll view owns the document; hand back whatever is
        // still mounted inside it. An explicit layout root lives among these.
        let mut document = alt.implicit_root.take().map(|mut view| view.take_child());
        let children = document
            .as_mut()
            .and_then(|document| {
                document
                    .as_any_mut()
                    .and_then(|any| any.downcast_mut::<crate::component::ImplicitDocument>())
            })
            .map(crate::component::ImplicitDocument::take_children)
            .unwrap_or_default();
        // The regular renderer starts from a blank slate, exactly like the
        // fresh `TuiMainScreen` a switch builds (`interactive-mode.ts:665`).
        self.reset_render_state();
        children
    }

    /// `implicitScrollView`, `tui-alt-screen.ts:261-268`. pi builds it in the
    /// constructor; here it is built the first time the alt screen is entered
    /// and then kept, because the mouse precedence compares its identity
    /// against a dispatch target that outlives the frame.
    ///
    /// The host's root children move into the document, which is why every
    /// `Tui` child operation reads them back out of
    /// [`Tui::root_children`] instead of a private list.
    fn ensure_implicit_root(&mut self) {
        if self.alt().is_some_and(|alt| alt.implicit_root.is_some()) {
            return;
        }
        let document = crate::component::ImplicitDocument::new(self.take_root_children());
        let view = crate::components::scroll_view::ScrollView::new(
            Box::new(document),
            crate::components::scroll_view::ScrollViewOptions {
                follow_end: true,
                primary: true,
                ..Default::default()
            },
        );
        self.alt_state().implicit_root = Some(Box::new(view));
    }

    fn alt_implicit_root_mut(&mut self) -> Option<&mut crate::components::scroll_view::ScrollView> {
        self.alt_screen.as_mut()?.implicit_root.as_deref_mut()
    }

    /// `beforeTerminalStop`, `tui-alt-screen.ts:369-383`.
    pub fn before_alt_screen_stop(&mut self) {
        self.close_search();
        // `:371-372`: a stop cancels the auto-scroll and forgets that a
        // selection press is in flight, so the timer cannot outlive the
        // alt screen it was started for.
        self.stop_selection_auto_scroll();
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.selection_press_active = false;
        }
        self.stop_scrollbar_hover();
        self.stop_scrollbar_drag();
        self.clear_alt_mouse_gesture();
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.flashes.dispose();
        }
        if !self.alt().is_some_and(|alt| alt.alt_screen_active) {
            return;
        }
        let mouse = self.alt().is_some_and(|alt| alt.options_mouse);
        // `deleteKittyImages`, `:421-423`: every image this alt screen
        // transmitted goes with it, so none is left behind on the terminal.
        let delete_images = if self
            .alt()
            .is_some_and(|alt| alt.image_protocol == crate::terminal_image::ImageProtocol::Kitty)
        {
            crate::terminal_image::delete_all_kitty_images()
        } else {
            String::new()
        };
        self.write_terminal(&format!(
            "{BEGIN_SYNCHRONIZED_OUTPUT}{delete_images}{}{ENABLE_AUTOWRAP}{END_SYNCHRONIZED_OUTPUT}",
            if mouse { DISABLE_MOUSE } else { "" }
        ));
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.uploaded_kitty_images.clear();
        }
    }

    /// `afterTerminalStop`, `tui-alt-screen.ts:385-419`.
    ///
    /// `preserve_screen` hands the terminal back untouched. Otherwise the
    /// document is repainted into the main screen so the transcript survives
    /// leaving fullscreen.
    pub fn after_alt_screen_stop(&mut self, options: TuiStopOptions) {
        if !self.alt().is_some_and(|alt| alt.alt_screen_active) {
            return;
        }
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.alt_screen_active = false;
        }
        if options.preserve_screen {
            self.write_terminal(&format!(
                "{BEGIN_SYNCHRONIZED_OUTPUT}{EXIT_ALT_SCREEN}\x1b[?25h{END_SYNCHRONIZED_OUTPUT}"
            ));
            self.restore_saved_capabilities();
            return;
        }
        let width = self.terminal_width().max(1);
        let document = self.alt_screen_render(width);
        let mut buffer = format!("{BEGIN_SYNCHRONIZED_OUTPUT}{EXIT_ALT_SCREEN}{DISABLE_AUTOWRAP}");
        for (row, line) in document.iter().enumerate() {
            if row > 0 {
                buffer.push_str("\r\n");
            }
            // `:403`: `render(width)` drops the shell-integration zone
            // prefix, `applyLineResets` normalises the line and adds the
            // segment reset. Colors survive both.
            let line = strip_osc133_zone_prefix(line);
            let line = line.replace(crate::ansi::CURSOR_MARKER, "");
            let line = crate::tui::apply_line_resets(vec![line])
                .pop()
                .unwrap_or_default();
            let line = if crate::terminal_image::is_image_line(&line)
                || crate::ansi::visible_width(&line) <= width
            {
                line
            } else {
                crate::ansi::slice_by_column(&line, 0, width, true)
            };
            buffer.push_str("\r\x1b[2K");
            buffer.push_str(&line);
        }
        {
            use std::fmt::Write as _;
            let _ = write!(
                buffer,
                "\x1b[0m{ENABLE_AUTOWRAP}\r\n\x1b[?25h{END_SYNCHRONIZED_OUTPUT}"
            );
        }
        self.write_terminal(&buffer);
        self.restore_saved_capabilities();
    }

    /// `savedCapabilities`, `:404-407`: put back what entering took away.
    fn restore_saved_capabilities(&mut self) {
        if let Some(saved) = self
            .alt_screen
            .as_mut()
            .and_then(|alt| alt.saved_capabilities.take())
        {
            crate::terminal_image::set_capabilities(saved);
        }
    }

    // ------------------------------------------------------------- root

    /// Mount the document the viewport lays out and scrolls. `setLayoutRoot`,
    /// `:320-326`.
    pub fn set_alt_screen_root(&mut self, component: Box<DynComponent>) {
        let id = self.replace_root_child(component);
        self.alt_state().root = Some(id);
        self.request_render();
    }

    /// The identity of the mounted root, if any.
    pub fn alt_screen_root(&self) -> Option<ComponentId> {
        self.alt().and_then(|alt| alt.root)
    }

    /// Drop the explicit layout root, back to the implicit scroll view.
    /// `setLayoutRoot(undefined)`, `interactive-mode.ts:672` — the host
    /// unmounts the chat viewport when it switches renderers.
    pub fn clear_alt_screen_root(&mut self) {
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.root = None;
        }
        self.request_render();
    }

    /// `render`, `:322-324`: the document, at the width the viewport gives it.
    ///
    /// `getRenderRoots`, `:332`: an explicit layout root wins; otherwise the
    /// root children *are* the document, stacked.
    pub fn alt_screen_render(&mut self, width: usize) -> Vec<String> {
        let root = self.alt().and_then(|alt| alt.root);
        match root {
            Some(root) => match self.component_by_id(root) {
                Some(component) => component.render(width),
                None => Vec::new(),
            },
            None => self.render_children(width),
        }
    }

    /// Lay the viewport out for one frame and record what the mouse and wheel
    /// routing need from it: the painted lines, the primary scroll view's
    /// identity, and the scroll updates the measuring pass deferred.
    ///
    /// The layout engine measures and mutates in two passes, so the frame is
    /// only usable once [`crate::layout::apply_scroll_updates`] has replayed
    /// `frame.scroll_updates`; this method does both and returns the frame
    /// with the boxes in their final positions.
    pub fn alt_screen_layout_frame(&mut self, width: usize, height: usize) -> LayoutFrame {
        let request: Option<RenderRequest> = Some(std::sync::Arc::new(|| {}));
        let root = self.alt().and_then(|alt| alt.root);
        let frame = if let Some(root) = root {
            let Some(component) = self.component_by_id_mut(root) else {
                self.set_primary_scroll_view(None);
                return empty_frame(width, height);
            };
            let frame = render_layout_frame(component, width, height, request);
            apply_scroll_updates(component, &frame.scroll_updates);
            frame
        } else {
            // `implicitScrollView`, `tui-alt-screen.ts:1675`: with no
            // explicit layout root the alt screen lays out the scroll view it
            // has owned since it was entered, whose document is the host's own
            // root children (`implicitDocument`, `:261-267`).
            let Some(root) = self.alt_implicit_root_mut() else {
                self.set_primary_scroll_view(None);
                return empty_frame(width, height);
            };
            let frame = render_layout_frame(&*root, width, height, request);
            apply_scroll_updates(&mut *root, &frame.scroll_updates);
            frame
        };
        self.set_primary_scroll_view(frame.primary_scroll_view);
        frame
    }

    // ------------------------------------------------------------- wheel

    /// The wheel delta for one report. `handleViewportInput`, `:700-703`.
    ///
    /// The accelerator produces the line count and the Alt multiplier is read
    /// off raw bit 3 of the button code *after* it — pi's order, which is why
    /// [`WheelScrollAccelerator`] knows nothing about Alt.
    pub fn alt_screen_wheel_delta(&mut self, wheel: &WheelEvent, now_ms: f64) -> i64 {
        let lines = self.alt_state().wheel_scroll.next(wheel.direction, now_ms);
        let multiplier = if wheel.button & 8 != 0 {
            ALT_WHEEL_SCROLL_MULTIPLIER
        } else {
            1
        };
        wheel.direction * lines * multiplier
    }

    /// `routeWheel`, `:985-996`.
    ///
    /// Every scroll view under the pointer is offered the remaining delta
    /// deepest-first; a view with `overscroll: contain` that stops consuming
    /// ends the walk, and the primary view takes whatever is left unless it
    /// was already offered it.
    pub fn alt_screen_route_wheel(
        &mut self,
        layout: &LayoutFrame,
        x: i64,
        y: i64,
        delta: i64,
    ) -> bool {
        let mut remaining = delta;
        let mut seen: Vec<ComponentId> = Vec::new();
        for scroll_view in get_scroll_views_at(layout, x, y) {
            seen.push(scroll_view);
            remaining -= self.scroll_view_by(remaining, scroll_view);
            if remaining == 0 {
                break;
            }
            if self.scroll_view_overscroll_is_contain(scroll_view) {
                break;
            }
        }
        if remaining != 0
            && let Some(primary) = layout.primary_scroll_view
            && !seen.contains(&primary)
        {
            remaining -= self.scroll_view_by(remaining, primary);
        }
        delta != remaining
    }

    fn scroll_view_by(&mut self, delta: i64, scroll_view: ComponentId) -> i64 {
        if delta == 0 {
            return 0;
        }
        match self.scroll_view_mut(scroll_view) {
            Some(view) => view.scroll_by(delta),
            None => 0,
        }
    }

    /// Downcast a mounted component to a `ScrollView`. pi holds live
    /// `ScrollView` references here (`tui-alt-screen.ts:988-996`); this is the
    /// only downcast in the tree.
    fn scroll_view_mut(
        &mut self,
        id: ComponentId,
    ) -> Option<&mut crate::components::scroll_view::ScrollView> {
        self.component_any_mut(id)?
            .downcast_mut::<crate::components::scroll_view::ScrollView>()
    }

    /// The terminal's current geometry.
    pub(crate) fn terminal_size(&self) -> (usize, usize) {
        self.terminal_size_inner()
    }

    fn set_scroll_view_active(&mut self, id: ComponentId, active: bool) {
        if let Some(view) = self.scroll_view_mut(id) {
            view.state.set_scrollbar_active(active);
        }
    }

    fn scroll_view_overscroll_is_contain(&mut self, scroll_view: ComponentId) -> bool {
        self.scroll_view_state(scroll_view).is_some_and(|state| {
            state.overscroll == crate::components::scroll_view::Overscroll::Contain
        })
    }

    fn scroll_view_state(&mut self, scroll_view: ComponentId) -> Option<ScrollViewStateSnapshot> {
        let view = self.scroll_view_mut(scroll_view)?;
        Some(ScrollViewStateSnapshot {
            overscroll: view.state.overscroll(),
            viewport_height: view.state.viewport_height(),
            scroll_top: view.state.scroll_top(),
        })
    }

    // ------------------------------------------------------ the precedence

    /// `handleMouseEvent`, `:893-954`.
    ///
    /// pi's order is load-bearing and is preserved exactly: a captured or
    /// pressed target swallows everything until release; then the search
    /// overlay's navigation buttons; then the overlay stack; then the
    /// scroll-to-end indicator and the scrollbar, but only when no overlay was
    /// hit; then the layout tree; and only then right-click paste and text
    /// selection.
    pub fn handle_alt_mouse_event(
        &mut self,
        layout: &LayoutFrame,
        raw: &SgrMouseEvent,
        now_ms: u64,
    ) -> MouseOutcome {
        let event = self.create_mouse_event(raw, None, None);
        let kind = event.kind;

        if let Some(target) = self
            .alt()
            .and_then(|alt| alt.mouse_capture.or(alt.mouse_press_target))
        {
            self.handle_gesture_mouse_event(raw, &event, target, now_ms);
            return MouseOutcome::Consumed;
        }

        if self.handle_search_mouse_event(raw) {
            return MouseOutcome::Consumed;
        }

        let overlay = self.dispatch_mouse_to_overlay(&event);
        if overlay.hit {
            self.stop_scrollbar_hover();
        } else {
            if self.handle_scroll_to_end_indicator_mouse_event(raw) {
                return MouseOutcome::Consumed;
            }
            let scrollbar_handled = self.handle_scrollbar_mouse_event(layout, raw);
            if self.alt().and_then(|alt| alt.scrollbar_drag).is_none() {
                self.update_scrollbar_hover(layout, raw.x, raw.y);
            }
            if scrollbar_handled {
                return MouseOutcome::Consumed;
            }
        }

        // pi: `overlay.result ?? (overlay.hit ? undefined : dispatchToLayout)` —
        // an overlay that swallowed the event stops the layout walk entirely.
        let result = match overlay.result {
            Some(result) => Some(result),
            None if overlay.hit => None,
            None => self.alt_screen_dispatch_mouse_to_layout(layout, &event),
        };
        if let Some(result) = result {
            let render = self.apply_alt_mouse_dispatch_result(&event, &result);
            if kind == MouseEventKind::Press {
                self.clear_alt_text_selection();
                let alt = self.alt_state();
                alt.mouse_press_target = Some(result.target);
                alt.mouse_press_point = Some((raw.x, raw.y));
                alt.mouse_press_moved = false;
            }
            if render {
                self.request_render();
            }
            return MouseOutcome::Consumed;
        }

        if self.handle_right_click_paste(raw) {
            return MouseOutcome::Consumed;
        }
        self.handle_selection_mouse_event(layout, raw, now_ms);
        MouseOutcome::Ignored
    }

    /// The captured/pressed-target branch. `tui-alt-screen.ts:896-926`.
    ///
    /// The release synthesises a click only when the press never moved and
    /// ended on the same cell — that is the whole of pi's click detection.
    fn handle_gesture_mouse_event(
        &mut self,
        raw: &SgrMouseEvent,
        event: &MouseEvent,
        target: MouseDispatchTarget,
        now_ms: u64,
    ) {
        let mut moved = self.alt().is_some_and(|alt| alt.mouse_press_moved);
        let point = self.alt().and_then(|alt| alt.mouse_press_point);
        if let Some((px, py)) = point
            && (raw.x != px || raw.y != py)
        {
            moved = true;
            let alt = self.alt_state();
            alt.mouse_press_moved = true;
            // `lastComponentClick = undefined`, `:919-920`: moving away
            // from the pressed cell breaks the double-click chain.
            alt.last_component_click.reset();
        }
        let forwarded = retarget_mouse_event(event, &target);
        let mut render = false;
        if let Some(result) = self.dispatch_mouse_by_id(target.component, &forwarded) {
            render = self.apply_alt_mouse_dispatch_result(event, &result);
        }
        if raw.release {
            if !moved && point == Some((raw.x, raw.y)) {
                let count = self.alt_state().last_component_click.next(
                    target.component,
                    raw.x,
                    raw.y,
                    now_ms,
                );
                let click_event =
                    self.create_mouse_event(raw, Some(MouseEventKind::Click), Some(count));
                let forwarded = retarget_mouse_event(&click_event, &target);
                if let Some(result) = self.dispatch_mouse_by_id(target.component, &forwarded) {
                    render = self.apply_alt_mouse_dispatch_result(&click_event, &result) || render;
                }
            }
            self.clear_alt_mouse_gesture();
        }
        if render {
            self.request_render();
        }
    }

    /// `dispatchMouseToLayout`, `:836-853`.
    ///
    /// Deepest, topmost box first, each component tried once. A component that
    /// is only a layout node — it has a node but no mouse handler of its own —
    /// is skipped, which is pi's `handleMouse === Container.prototype.
    /// handleMouse` test.
    pub fn alt_screen_dispatch_mouse_to_layout(
        &mut self,
        layout: &LayoutFrame,
        event: &MouseEvent,
    ) -> Option<MouseDispatchResult> {
        let hits: Vec<(ComponentId, i64, i64, usize, usize)> =
            get_layout_boxes_at(layout, event.screen_x, event.screen_y)
                .into_iter()
                .filter_map(|box_| {
                    let id = box_.component?;
                    Some((
                        id,
                        box_.rect.x,
                        box_.rect.y,
                        box_.rect.width.max(0) as usize,
                        box_.rect.height.max(0) as usize,
                    ))
                })
                .collect();
        let mut visited: Vec<ComponentId> = Vec::new();
        for (id, x, y, width, height) in hits {
            if visited.contains(&id) || self.component_is_layout_only(id) {
                continue;
            }
            visited.push(id);
            let forwarded = retarget_mouse_event(
                event,
                &MouseDispatchTarget {
                    component: id,
                    origin_x: x,
                    origin_y: y,
                    width,
                    height,
                },
            );
            if let Some(result) = self.dispatch_mouse_by_id(id, &forwarded) {
                return Some(result);
            }
        }
        None
    }

    /// `applyMouseDispatchResult`, `:845-858`.
    ///
    /// Focus follows the resolved owner — an overlay keeps focus when one of
    /// its own components is clicked — a `capture` result arms the gesture
    /// target, and the render default is press/click/drag/wheel plus any focus
    /// change.
    pub fn apply_alt_mouse_dispatch_result(
        &mut self,
        event: &MouseEvent,
        result: &MouseDispatchResult,
    ) -> bool {
        let owner = result.focus_target.unwrap_or(result.target.component);
        let focus_target = self.resolve_mouse_focus_target(owner);
        let focus_changed = result.focus == Some(true) && self.focused_id_value() != focus_target;
        if result.focus == Some(true) {
            self.set_focus_by_id(focus_target);
        }
        if result.capture == Some(true) {
            self.alt_state().mouse_capture = Some(result.target);
        }
        result.wants_render(event, focus_changed)
    }

    /// `getSelectionPoint`, `:1204-1214`, and `getScrollSelectionPoint`,
    /// `:1155-1177`.
    ///
    /// A point inside a scroll view is resolved against that view's content —
    /// clipped to its rect, then mapped through its scroll offset — so a
    /// selection follows the content under the pointer rather than the screen.
    /// Anything else falls back to raw screen coordinates clamped to the
    /// terminal.
    fn selection_point(
        &self,
        layout: &LayoutFrame,
        raw: &SgrMouseEvent,
        anchor_scroll_view: Option<ComponentId>,
    ) -> SelectionPoint {
        if let Some(scroll_view) = anchor_scroll_view
            && let Some(point) = self.scroll_selection_point(layout, scroll_view, raw.x, raw.y)
        {
            return point;
        }
        let (columns, rows) = self.terminal_size();
        SelectionPoint {
            row: raw.y.clamp(0, rows.saturating_sub(1) as i64),
            col: raw.x.clamp(0, columns.saturating_sub(1) as i64),
            scroll_view: None,
            boundary: false,
        }
    }

    /// `getScrollSelectionPoint`, `:1155-1177`.
    fn scroll_selection_point(
        &self,
        layout: &LayoutFrame,
        scroll_view: ComponentId,
        x: i64,
        y: i64,
    ) -> Option<SelectionPoint> {
        let box_ = get_scroll_view_box(layout, scroll_view)?;
        if box_.rect.height <= 0 || box_.clip.height <= 0 {
            return None;
        }
        let (_, rows) = self.terminal_size();
        let visible_top = 0.max(box_.rect.y).max(box_.clip.y);
        let visible_bottom = (rows as i64 - 1)
            .min(box_.rect.y + box_.rect.height - 1)
            .min(box_.clip.y + box_.clip.height - 1);
        if visible_bottom < visible_top {
            return None;
        }
        let pointer_row = visible_top.max(visible_bottom.min(y));
        let content_lines = box_.scroll_content_lines.as_ref().map_or(1, Vec::len);
        let max_content_row = content_lines.saturating_sub(1) as i64;
        let scroll_top = box_
            .scroll
            .as_ref()
            .map_or(0, |scroll| scroll.scroll_top as i64);
        Some(SelectionPoint {
            row: (scroll_top + pointer_row - box_.rect.y).clamp(0, max_content_row.max(0)),
            col: (x - box_.rect.x).clamp(0, (box_.rect.width - 1).max(0)),
            scroll_view: Some(scroll_view),
            boundary: false,
        })
    }

    /// `getClickCount`, `:1232-1256`.
    ///
    /// Only a click that landed on the *same word* continues the chain, and
    /// without a word range the chain never starts, so a click on bare text is
    /// always count 1. pi also drops the remembered click there
    /// (`lastClick = undefined`), so a click on a space in the middle of a
    /// chain does not leave a stale count to be picked up by the next click.
    fn get_click_count(
        &mut self,
        point: SelectionPoint,
        word: Option<&SelectionRange>,
        now_ms: u64,
    ) -> u32 {
        let Some(word) = word else {
            self.alt_state().last_click = None;
            return 1;
        };
        let previous = self.alt().and_then(|alt| alt.last_click);
        let count = match previous {
            Some((previous, timestamp))
                if now_ms.saturating_sub(timestamp) <= DOUBLE_CLICK_INTERVAL_MS
                    && previous.row == point.row
                    && previous.scroll_view == point.scroll_view
                    && previous.word_start == word.start.col
                    && previous.word_end == word.end.col =>
            {
                (previous.count % 3) + 1
            }
            _ => 1,
        };
        let alt = self.alt_state();
        alt.last_click = Some((
            ClickTarget {
                count,
                row: point.row,
                scroll_view: point.scroll_view,
                word_start: word.start.col,
                word_end: word.end.col,
            },
            now_ms,
        ));
        count
    }

    /// `getSelectionBounds`, `:1393-1410`.
    fn selection_bounds(&self) -> Option<(SelectionPoint, SelectionPoint)> {
        let anchor = self.alt()?.selection_anchor?;
        let focus = self.alt()?.selection_focus?;
        if anchor.scroll_view != focus.scroll_view
            || (anchor.row == focus.row && anchor.col == focus.col)
        {
            return None;
        }
        let anchor_first = (anchor.row, anchor.col) < (focus.row, focus.col);
        Some(if anchor_first {
            (anchor, focus)
        } else {
            (focus, anchor)
        })
    }

    /// `getSelectionSourceLine`, `:1159-1165`.
    ///
    /// A point inside a scroll view reads that view's *content* lines, so a
    /// selection made while scrolled up follows the document rather than the
    /// screen; every other point reads the last painted frame
    /// (`previousScreen`).
    fn selection_source_line(&self, layout: &LayoutFrame, point: &SelectionPoint) -> String {
        if let Some(scroll_view) = point.scroll_view
            && let Some(lines) = get_scroll_view_box(layout, scroll_view)
                .and_then(|box_| box_.scroll_content_lines.as_ref())
        {
            return lines
                .get(point.row.max(0) as usize)
                .cloned()
                .unwrap_or_default();
        }
        self.alt()
            .and_then(|alt| alt.previous_screen.get(point.row.max(0) as usize))
            .cloned()
            .unwrap_or_default()
    }

    /// `TERMINAL_WORD_SELECTION_JOINERS`, `:84`: `/` and `-` keep the token
    /// whole on a double click, like the terminal emulator would.
    const WORD_SELECTION_JOINERS: [char; 2] = ['/', '-'];

    /// `getWordSelection`, `:1167-1202`.
    ///
    /// The clicked segment grows across joiners into the whole token. This
    /// crate's segmenter is the same approximation the editor already uses
    /// (`word_navigation::segment_words`), so ASCII tokens — the ones pi's
    /// tests pin — behave identically.
    fn word_selection(
        &self,
        layout: &LayoutFrame,
        point: &SelectionPoint,
    ) -> Option<SelectionRange> {
        let line =
            crate::ansi::strip_terminal_sequences(&self.selection_source_line(layout, point));
        let mut segments: Vec<(usize, usize, bool, bool)> = Vec::new();
        let mut start = 0usize;
        for segment in crate::word_navigation::segment_words(&line) {
            let end = start + crate::ansi::visible_width(&segment.text);
            let joiner =
                Self::WORD_SELECTION_JOINERS.contains(&segment.text.chars().next().unwrap_or(' '));
            segments.push((start, end, segment.is_word_like || joiner, joiner));
            start = end;
        }
        let clicked = segments
            .iter()
            .position(|(start, end, _, _)| point.col >= *start as i64 && point.col < *end as i64)?;
        let can_join = |left: &(usize, usize, bool, bool), right: &(usize, usize, bool, bool)| {
            left.2 && right.2 && (left.3 || right.3)
        };
        let mut selection_start = segments[clicked].0;
        let mut selection_end = segments[clicked].1;
        for index in (1..=clicked).rev() {
            if !can_join(&segments[index - 1], &segments[index]) {
                break;
            }
            selection_start = segments[index - 1].0;
        }
        for index in clicked..segments.len().saturating_sub(1) {
            if !can_join(&segments[index], &segments[index + 1]) {
                break;
            }
            selection_end = segments[index + 1].1;
        }
        Some(SelectionRange {
            start: SelectionPoint {
                col: selection_start as i64,
                ..*point
            },
            end: SelectionPoint {
                col: selection_end as i64,
                boundary: true,
                ..*point
            },
        })
    }

    /// `getLineSelection`, `:1204-1210`.
    fn line_selection(&self, layout: &LayoutFrame, point: &SelectionPoint) -> SelectionRange {
        let width = crate::ansi::visible_width(&self.selection_source_line(layout, point)) as i64;
        SelectionRange {
            start: SelectionPoint { col: 0, ..*point },
            end: SelectionPoint {
                col: width,
                boundary: true,
                ..*point
            },
        }
    }

    /// `updateSelectionFocus`, `:1212-1230`.
    ///
    /// Character granularity simply follows the pointer. A word or line drag
    /// snaps to the range under the pointer and keeps the *pressed* range as
    /// the fixed end, so dragging backwards extends the selection the other
    /// way instead of swapping the ends.
    fn update_selection_focus(&mut self, layout: &LayoutFrame, point: SelectionPoint) {
        let Some(alt) = self.alt() else { return };
        if alt.selection_granularity == SelectionGranularity::Character
            || alt.selection_initial_range.is_none()
        {
            self.alt_state().selection_focus = Some(point);
            return;
        }
        let granularity = alt.selection_granularity;
        let Some(initial) = alt.selection_initial_range else {
            return;
        };
        let range = match granularity {
            SelectionGranularity::Word => self.word_selection(layout, &point),
            SelectionGranularity::Line => Some(self.line_selection(layout, &point)),
            SelectionGranularity::Character => None,
        };
        let Some(range) = range else { return };
        let target_before_initial =
            (range.start.row, range.start.col) < (initial.start.row, initial.start.col);
        let alt = self.alt_state();
        if target_before_initial {
            alt.selection_anchor = Some(initial.end);
            alt.selection_focus = Some(range.start);
        } else {
            alt.selection_anchor = Some(initial.start);
            alt.selection_focus = Some(range.end);
        }
    }

    /// `getSelectionColumns`, `:1410-1429`.
    ///
    /// The start snaps back to a grapheme boundary and the end forward to
    /// one, which is what keeps a drag over CJK, emoji or combining marks
    /// from cutting a cluster in half. A boundary end is already exclusive.
    fn selection_columns(
        line: &str,
        row: i64,
        selection: &(SelectionPoint, SelectionPoint),
        min_column: i64,
        max_column: i64,
    ) -> (usize, usize) {
        let line_width = crate::ansi::visible_width(line);
        let mut start = min_column.max(0);
        let mut end = max_column.min(line_width as i64);
        if row == selection.0.row {
            let column = selection.0.col;
            start = crate::ansi::grapheme_cell_range(line, column.max(0) as usize)
                .map_or_else(|| column.min(line_width as i64), |(from, _)| from as i64);
        }
        if row == selection.1.row {
            let column = selection.1.col;
            end = if selection.1.boundary {
                column.min(line_width as i64)
            } else {
                crate::ansi::grapheme_cell_range(line, column.max(0) as usize)
                    .map_or_else(|| (column + 1).min(line_width as i64), |(_, to)| to as i64)
            };
        }
        (
            start.max(min_column).max(0) as usize,
            end.min(max_column.max(0)).max(0) as usize,
        )
    }

    /// `getActiveSelectionText`, `:1431-1453`.
    ///
    /// Returns `None` for an empty selection, exactly as pi does, so
    /// `hasActiveSelection` and the copy-on-select path agree without a
    /// second emptiness test.
    pub fn alt_screen_selection_text(&self, layout: &LayoutFrame) -> Option<String> {
        let (start, end) = self.selection_bounds()?;
        let mut source: Option<&[String]> = None;
        if let Some(scroll_view) = start.scroll_view {
            source = get_scroll_view_box(layout, scroll_view)
                .and_then(|box_| box_.scroll_content_lines.as_deref());
            source?;
        }
        let previous = match self.alt() {
            Some(alt) => alt.previous_screen.as_slice(),
            None => &[],
        };
        let source = source.unwrap_or(previous);
        let selection = (start, end);
        let mut lines: Vec<String> = Vec::new();
        for row in start.row..=end.row {
            let line = source.get(row.max(0) as usize).map_or("", String::as_str);
            let (from, to) = Self::selection_columns(
                line,
                row,
                &selection,
                0,
                crate::ansi::visible_width(line) as i64,
            );
            let sliced = crate::ansi::slice_by_column(line, from, to.saturating_sub(from), true);
            lines.push(
                crate::ansi::strip_terminal_sequences(&sliced)
                    .trim_end()
                    .to_string(),
            );
        }
        let text = lines.join("\n");
        (!text.is_empty()).then_some(text)
    }

    /// Refresh the cached selection text so a host can read and copy it.
    /// pi computes the same value lazily from the same state.
    fn refresh_selection_text(&mut self, layout: &LayoutFrame) {
        let text = self.alt_screen_selection_text(layout);
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.selection_text = text;
        }
    }

    /// `updateSelectionAutoScroll`, `:1258-1279`.
    ///
    /// Records where the drag is and which way it should drift. pi then arms a
    /// 50 ms interval; this crate owns no timer, so the host drives
    /// [`Tui::alt_screen_auto_scroll_selection`] instead — the same
    /// host-deadline pattern `stdin_buffer` uses.
    fn update_selection_auto_scroll(&mut self, layout: &LayoutFrame, raw: &SgrMouseEvent) {
        let Some(scroll_view) = self
            .alt()
            .and_then(|alt| alt.selection_anchor)
            .and_then(|anchor| anchor.scroll_view)
        else {
            self.stop_selection_auto_scroll();
            return;
        };
        let Some((visible_top, visible_bottom)) =
            self.scroll_view_visible_rows(layout, scroll_view)
        else {
            self.stop_selection_auto_scroll();
            return;
        };
        let direction = if raw.y <= visible_top {
            -1
        } else {
            i8::from(raw.y >= visible_bottom)
        };
        let alt = self.alt_state();
        alt.selection_drag_pointer = Some((raw.x, raw.y));
        alt.selection_auto_scroll_direction = direction;
        if direction == 0 {
            self.stop_selection_auto_scroll();
        }
    }

    /// `stopSelectionAutoScroll`, `:1298-1304`.
    pub fn stop_selection_auto_scroll(&mut self) {
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.selection_auto_scroll_direction = 0;
            alt.selection_drag_pointer = None;
        }
    }

    /// The visible row span of a scroll view. pi inlines this twice
    /// (`:1160-1166`, `:1270-1276`).
    fn scroll_view_visible_rows(
        &self,
        layout: &LayoutFrame,
        scroll_view: ComponentId,
    ) -> Option<(i64, i64)> {
        let box_ = get_scroll_view_box(layout, scroll_view)?;
        if box_.rect.height <= 0 || box_.clip.height <= 0 {
            return None;
        }
        let (_, rows) = self.terminal_size();
        let visible_top = 0.max(box_.rect.y).max(box_.clip.y);
        let visible_bottom = (rows as i64 - 1)
            .min(box_.rect.y + box_.rect.height - 1)
            .min(box_.clip.y + box_.clip.height - 1);
        (visible_bottom >= visible_top).then_some((visible_top, visible_bottom))
    }

    /// `autoScrollSelection`, `:1281-1296`.
    ///
    /// One tick of pi's 50 ms interval: drift by a line, re-derive the focus
    /// point from the pointer, and stop when the view will not move. Returns
    /// whether it moved, so a host knows to keep ticking.
    pub fn alt_screen_auto_scroll_selection(&mut self, layout: &LayoutFrame) -> bool {
        let Some(scroll_view) = self
            .alt()
            .and_then(|alt| alt.selection_anchor)
            .and_then(|anchor| anchor.scroll_view)
        else {
            self.stop_selection_auto_scroll();
            return false;
        };
        let Some(pointer) = self.alt().and_then(|alt| alt.selection_drag_pointer) else {
            self.stop_selection_auto_scroll();
            return false;
        };
        let direction = self
            .alt()
            .map_or(0, |alt| alt.selection_auto_scroll_direction);
        if direction == 0 {
            self.stop_selection_auto_scroll();
            return false;
        }
        let remaining = self.scroll_view_by(direction as i64, scroll_view);
        if remaining == direction as i64 {
            self.stop_selection_auto_scroll();
            return false;
        }
        if let Some(point) = self.scroll_selection_point(layout, scroll_view, pointer.0, pointer.1)
        {
            self.update_selection_focus(layout, point);
        }
        self.refresh_selection_text(layout);
        self.request_render();
        true
    }

    /// `handleSelectionMouseEvent`, `:1313-1391`.
    ///
    /// This is where a click on a component that ignored the press is born:
    /// the editor deliberately leaves press, drag and release unhandled
    /// (`components/editor.ts:653-657`), and pi's renderer synthesises the
    /// click here from a press and release on the same cell. It is also where
    /// word, line and OSC 8 link selection are decided.
    // Selection state machine mirrors upstream `handleSelectionMouseEvent`;
    // splitting it would obscure the port.
    #[allow(clippy::too_many_lines)]
    fn handle_selection_mouse_event(
        &mut self,
        layout: &LayoutFrame,
        raw: &SgrMouseEvent,
        now_ms: u64,
    ) {
        let button = raw.button & 3;
        // A release carries button 3 when the terminal cannot name the button
        // (`tui-alt-screen.ts:1314`).
        if button != 0 && !(raw.release && button == 3) {
            return;
        }
        let anchor_scroll_view = self
            .alt()
            .and_then(|alt| alt.selection_anchor)
            .and_then(|anchor| anchor.scroll_view);
        let point = self.selection_point(layout, raw, anchor_scroll_view);

        if raw.release {
            if !self.alt().is_some_and(|alt| alt.selection_press_active) {
                return;
            }
            self.alt_state().selection_press_active = false;
            self.stop_selection_auto_scroll();
            let Some(_) = self.alt().and_then(|alt| alt.selection_anchor) else {
                return;
            };
            self.update_selection_focus(layout, point);
            let is_click = self.alt().is_some_and(|alt| {
                let Some(anchor) = alt.selection_anchor else {
                    return false;
                };
                !alt.selection_dragged
                    && anchor.scroll_view == point.scroll_view
                    && anchor.row == point.row
                    && anchor.col == point.col
            });
            let clicked_url = if is_click {
                self.alt().and_then(|alt| alt.pressed_url.clone())
            } else {
                None
            };
            self.alt_state().pressed_url = None;
            if let Some(url) = clicked_url
                && self.alt().is_some_and(|alt| alt.open_url.is_some())
            {
                self.alt_state().selection_anchor = None;
                self.alt_state().selection_focus = None;
                // Best-effort: a host that throws must not take the renderer
                // down (`:1334-1338`).
                if let Some(open) = self.alt_state().open_url.take() {
                    open(url);
                    self.alt_state().open_url = Some(open);
                }
                self.request_render();
                return;
            }
            if is_click {
                let count = self
                    .alt()
                    .and_then(|alt| alt.last_click)
                    .map_or(1, |(target, _)| target.count);
                let click_event =
                    self.create_mouse_event(raw, Some(MouseEventKind::Click), Some(count));
                let overlay = self.dispatch_mouse_to_overlay(&click_event);
                let result = match overlay.result {
                    Some(result) => Some(result),
                    None if overlay.hit => None,
                    None => self.alt_screen_dispatch_mouse_to_layout(layout, &click_event),
                };
                if let Some(result) = result {
                    let render = self.apply_alt_mouse_dispatch_result(&click_event, &result);
                    self.clear_alt_text_selection();
                    if render {
                        self.request_render();
                    }
                    return;
                }
            }
            // `copySelectionToClipboard`, `:1455-1459`: the text comes from
            // the same bounds the highlight uses, and the copy itself is the
            // host's (this crate owns no clipboard).
            self.refresh_selection_text(layout);
            if self.alt().is_some_and(|alt| alt.copy_on_select)
                && self.alt_screen_has_active_selection()
            {
                self.alt_state().copy_requested = true;
            }
            self.request_render();
            return;
        }
        if raw.button & 32 != 0 {
            if !self
                .alt()
                .is_some_and(|alt| alt.selection_press_active && alt.selection_anchor.is_some())
            {
                return;
            }
            self.alt_state().selection_dragged = true;
            self.alt_state().last_click = None;
            self.alt_state().pressed_url = None;
            self.update_selection_focus(layout, point);
            self.update_selection_auto_scroll(layout, raw);
            self.refresh_selection_text(layout);
            self.request_render();
            return;
        }

        self.stop_selection_auto_scroll();
        // `!this.hasOverlay() && this.currentLayout`, `:1372-1374`: with an
        // overlay up the anchor is taken in screen coordinates, not against
        // the transcript's own scroll offset.
        let scroll_view = if self.has_overlay() {
            None
        } else {
            get_scroll_views_at(layout, raw.x, raw.y).first().copied()
        };
        let anchor = self.selection_point(layout, raw, scroll_view);
        let word = self.word_selection(layout, &anchor);
        let count = self.get_click_count(anchor, word.as_ref(), now_ms);
        let range = match count {
            2 => word,
            3 => Some(self.line_selection(layout, &anchor)),
            _ => None,
        };
        let (_, rows) = self.terminal_size();
        let (_, columns) = self.terminal_size();
        let line = self
            .alt()
            .and_then(|alt| {
                alt.previous_screen
                    .get(raw.y.clamp(0, rows.saturating_sub(1) as i64) as usize)
            })
            .cloned()
            .unwrap_or_default();
        let pressed_url = if range.is_some() {
            None
        } else {
            crate::ansi::osc8_link_at_column(
                &line,
                raw.x.clamp(0, columns.saturating_sub(1) as i64) as usize,
            )
        };
        let alt = self.alt_state();
        alt.selection_press_active = true;
        alt.selection_granularity = match (range, count) {
            (Some(_), 3) => SelectionGranularity::Line,
            (Some(_), _) => SelectionGranularity::Word,
            (None, _) => SelectionGranularity::Character,
        };
        alt.selection_initial_range = range;
        alt.selection_anchor = range.as_ref().map(|range| range.start).or(Some(anchor));
        alt.selection_focus = range.as_ref().map(|range| range.end).or(Some(anchor));
        alt.selection_dragged = false;
        alt.pressed_url = pressed_url;
        self.refresh_selection_text(layout);
        self.request_render();
    }

    /// `clearComponentMouseGesture`, `:664-669`.
    pub fn clear_alt_mouse_gesture(&mut self) {
        let alt = self.alt_state();
        alt.mouse_capture = None;
        alt.mouse_press_target = None;
        alt.mouse_press_point = None;
        alt.mouse_press_moved = false;
    }

    /// `dispatchMouseToOverlay`, `tui.ts:861-882`.
    ///
    /// The visually topmost overlay whose rectangle contains the point claims
    /// the event; a component that does not handle it still counts as a hit,
    /// which is what stops the layout walk underneath.
    pub fn dispatch_mouse_to_overlay(&mut self, event: &MouseEvent) -> AltOverlayHit {
        for (id, bounds, component) in self.rendered_overlays_topmost_first() {
            if !contains(bounds, event.screen_x, event.screen_y) {
                continue;
            }
            let forwarded = retarget_mouse_event(
                event,
                &MouseDispatchTarget {
                    component,
                    origin_x: bounds.col,
                    origin_y: bounds.row,
                    width: bounds.width,
                    height: bounds.height,
                },
            );
            let result = self
                .dispatch_mouse_by_id(component, &forwarded)
                .map(|mut result| {
                    // pi pins the focus target on the overlay itself
                    // (`tui.ts:879-880`), so clicking a nested control does not
                    // move the keyboard off the overlay that owns it.
                    if result.focus == Some(true) {
                        result.focus_target = Some(component);
                    }
                    result
                });
            return AltOverlayHit {
                hit: true,
                overlay: Some(id),
                result,
            };
        }
        AltOverlayHit::default()
    }

    /// `createMouseEvent`, `:801-825`.
    pub fn create_mouse_event(
        &self,
        raw: &SgrMouseEvent,
        kind_override: Option<MouseEventKind>,
        click_count: Option<u32>,
    ) -> MouseEvent {
        let (width, height) = self.terminal_size();
        MouseEvent::new(
            kind_override.unwrap_or_else(|| event_kind_for(raw)),
            raw.button,
            raw.x,
            raw.y,
            width,
            height,
        )
        .with_optional_click_count(click_count)
    }

    /// `handleRightClickPaste`, `:1010-1025`.
    ///
    /// Windows-only, and never under VS Code's integrated terminal, which
    /// already pastes on right click.
    pub fn handle_right_click_paste(&mut self, raw: &SgrMouseEvent) -> bool {
        if self
            .alt()
            .is_none_or(|alt| alt.on_right_click_paste.is_none())
            || !cfg!(target_os = "windows")
            || raw.release
            || raw.button != 2
        {
            return false;
        }
        if std::env::var("TERM_PROGRAM").is_ok_and(|program| program.to_lowercase() == "vscode") {
            return false;
        }
        // Best-effort: a clipboard failure must not take the renderer down.
        if let Some(paste) = self.alt_state().on_right_click_paste.take() {
            paste();
            self.alt_state().on_right_click_paste = Some(paste);
        }
        true
    }

    /// `handleScrollToEndIndicatorMouseEvent`, `:1028-1034`.
    pub fn handle_scroll_to_end_indicator_mouse_event(&mut self, raw: &SgrMouseEvent) -> bool {
        let Some(rect) = self.alt().and_then(|alt| alt.scroll_to_end_indicator_rect) else {
            return false;
        };
        if raw.release || raw.button & 32 != 0 || raw.button & 3 != 0 {
            return false;
        }
        if raw.y != rect.row || raw.x < rect.column || raw.x >= rect.column + rect.width {
            return false;
        }
        self.alt_screen_scroll_to_bottom();
        true
    }

    // --------------------------------------------------------- scrollbars

    /// `getScrollbarTargetAt`, `:1036-1051`.
    ///
    /// Nothing while an overlay is up, and an auto-hiding scrollbar only
    /// counts for hover (`include_hidden_auto`).
    pub fn get_scrollbar_target_at(
        &self,
        layout: &LayoutFrame,
        x: i64,
        y: i64,
        include_hidden_auto: bool,
    ) -> Option<(ComponentId, ScrollbarGeometry)> {
        if self.has_overlay() {
            return None;
        }
        for scroll_view in get_scroll_views_at(layout, x, y) {
            let Some(box_) = get_scroll_view_box(layout, scroll_view) else {
                continue;
            };
            let Some(geometry) = get_scrollbar_geometry(box_, include_hidden_auto) else {
                continue;
            };
            if x == geometry.column as i64
                && y >= geometry.track_top as i64
                && y < (geometry.track_top + geometry.track_height) as i64
            {
                return Some((scroll_view, geometry));
            }
        }
        None
    }

    /// `setScrollbarHover`, `:1053-1058`.
    pub fn set_scrollbar_hover(&mut self, scroll_view: Option<ComponentId>) {
        let current = self.alt().and_then(|alt| alt.scrollbar_hover);
        if current == scroll_view {
            return;
        }
        if let Some(previous) = current {
            self.set_scroll_view_active(previous, false);
        }
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.scrollbar_hover = scroll_view;
        }
        if let Some(next) = scroll_view {
            self.set_scroll_view_active(next, true);
        }
    }

    /// `updateScrollbarHover`, `:1060-1062`.
    pub fn update_scrollbar_hover(&mut self, layout: &LayoutFrame, x: i64, y: i64) {
        let target = self
            .get_scrollbar_target_at(layout, x, y, true)
            .map(|(id, _)| id);
        self.set_scrollbar_hover(target);
    }

    /// `stopScrollbarHover`, `:1064-1066`.
    pub fn stop_scrollbar_hover(&mut self) {
        self.set_scrollbar_hover(None);
    }

    /// `stopScrollbarDrag`, `:1125-1127`.
    pub fn stop_scrollbar_drag(&mut self) {
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.scrollbar_drag = None;
        }
    }

    /// `scrollScrollbarToPointer`, `:1068-1078`.
    pub fn scroll_scrollbar_to_pointer(
        &mut self,
        scroll_view: ComponentId,
        geometry: &ScrollbarGeometry,
        pointer_y: i64,
        grab_offset: i64,
    ) {
        let max_thumb_offset = geometry.track_height.saturating_sub(geometry.thumb_height) as i64;
        let thumb_offset =
            (pointer_y - geometry.track_top as i64 - grab_offset).clamp(0, max_thumb_offset);
        let scroll_top = if max_thumb_offset == 0 {
            0
        } else {
            ((thumb_offset as f64 / max_thumb_offset as f64) * geometry.max_scroll_top as f64)
                .round() as usize
        };
        if let Some(view) = self.scroll_view_mut(scroll_view) {
            view.scroll_to(scroll_top);
        }
        self.request_render();
    }

    /// `handleScrollbarMouseEvent`, `:1080-1123`.
    ///
    /// A press on the track jumps so the pointer lands mid-thumb; a press on
    /// the thumb grabs it. Either way a drag is armed that swallows every
    /// later event until the release.
    pub fn handle_scrollbar_mouse_event(
        &mut self,
        layout: &LayoutFrame,
        raw: &SgrMouseEvent,
    ) -> bool {
        if let Some(drag) = self.alt().and_then(|alt| alt.scrollbar_drag) {
            if raw.release {
                self.stop_scrollbar_drag();
                return true;
            }
            let geometry = get_scroll_view_box(layout, drag.scroll_view)
                .and_then(|box_| get_scrollbar_geometry(box_, false));
            if let Some(geometry) = geometry {
                self.scroll_scrollbar_to_pointer(
                    drag.scroll_view,
                    &geometry,
                    raw.y,
                    drag.grab_offset,
                );
            }
            return true;
        }
        if raw.release || raw.button & 32 != 0 || raw.button & 3 != 0 {
            return false;
        }
        let Some((scroll_view, geometry)) =
            self.get_scrollbar_target_at(layout, raw.x, raw.y, false)
        else {
            return false;
        };
        // A scrollbar press ends any text selection in progress, and unlike
        // `clearTextSelection` it also breaks the click chain (`:1105-1112`).
        self.clear_alt_text_selection();
        self.alt_state().last_click = None;
        self.set_scrollbar_hover(Some(scroll_view));
        let on_thumb = raw.y >= geometry.thumb_top as i64
            && raw.y < (geometry.thumb_top + geometry.thumb_height) as i64;
        let grab_offset = if on_thumb {
            raw.y - geometry.thumb_top as i64
        } else {
            (geometry.thumb_height / 2) as i64
        };
        if !on_thumb {
            self.scroll_scrollbar_to_pointer(scroll_view, &geometry, raw.y, grab_offset);
        }
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.scrollbar_drag = Some(ScrollbarDrag {
                scroll_view,
                grab_offset,
            });
        }
        true
    }

    // ----------------------------------------------------------- scrolling

    /// `scrollBy`, `:482-485`.
    pub fn alt_screen_scroll_by(&mut self, lines: i64) {
        if let Some(view) = self.primary_scroll_view_mut() {
            view.scroll_by(lines);
        }
        self.request_render();
    }

    /// `scrollToTop`, `:487-490`.
    pub fn alt_screen_scroll_to_top(&mut self) {
        if let Some(view) = self.primary_scroll_view_mut() {
            view.scroll_to_start();
        }
        self.request_render();
    }

    /// `scrollToBottom`, `:492-495`.
    pub fn alt_screen_scroll_to_bottom(&mut self) {
        if let Some(view) = self.primary_scroll_view_mut() {
            view.scroll_to_end();
        }
        self.request_render();
    }

    /// `getPrimaryScrollView`, `:335-337`: the primary from the last layout
    /// pass, else the implicit view every alt screen owns since it was
    /// entered. pi reads `currentLayout?.primaryScrollView ??
    /// this.implicitScrollView` — the fallback matters before the first paint
    /// and whenever the layout root has no scroll view of its own.
    pub(crate) fn primary_scroll_view_mut(
        &mut self,
    ) -> Option<&mut crate::components::scroll_view::ScrollView> {
        match self.primary_scroll_view_id() {
            Some(primary) => self.scroll_view_mut(primary),
            None => self.alt_implicit_root_mut(),
        }
    }

    /// The primary view's scroll offset, `getPrimaryScrollView().scrollTop`.
    pub fn alt_screen_viewport_top(&mut self) -> usize {
        let Some(primary) = self.primary_scroll_view_id() else {
            return self
                .alt_implicit_root_mut()
                .map_or(0, |view| view.state.scroll_top());
        };
        self.scroll_view_state(primary)
            .map_or(0, |state| state.scroll_top)
    }

    /// `getPrimaryScrollView().viewportHeight`, `:748`.
    fn primary_viewport_height(&mut self) -> usize {
        if let Some(primary) = self.primary_scroll_view_id() {
            return self
                .scroll_view_state(primary)
                .map_or(0, |state| state.viewport_height);
        }
        self.alt_implicit_root_mut()
            .map_or(0, |view| view.state.viewport_height())
    }

    /// `scrollToPrompt`, `:497-509`: the next or previous shell prompt marker
    /// (`OSC 133;A`) above or below the viewport, which is how a fullscreen
    /// transcript jumps between turns.
    pub fn alt_screen_scroll_to_prompt(&mut self, direction: i64) {
        if direction == 0 {
            return;
        }
        let frame = self.current_layout_frame();
        let Some(primary) = self.primary_scroll_view_id() else {
            return;
        };
        let Some(lines) =
            get_scroll_view_box(&frame, primary).and_then(|box_| box_.scroll_content_lines.clone())
        else {
            return;
        };
        let start = self
            .scroll_view_state(primary)
            .map_or(0, |state| state.scroll_top) as i64;
        let mut row = start + direction;
        while row >= 0 && (row as usize) < lines.len() {
            if is_osc133_prompt_start(&lines[row as usize]) {
                if let Some(view) = self.scroll_view_mut(primary) {
                    view.scroll_to(row as usize);
                }
                self.request_render();
                return;
            }
            row += direction;
        }
    }

    // ------------------------------------------------------- viewport input

    /// `handleViewportInput`, `:671-786`.
    ///
    /// pi registers this listener in the alt screen's constructor (`:280`), so
    /// the bytes are offered to the viewport before any focused component sees
    /// them: focus reports, the wheel, SGR and X10 mouse reports, and the
    /// `tui.altScreen.*` keys. `true` means the byte was consumed and nothing
    /// downstream should look at it.
    ///
    /// `now_ms` is the host clock. This crate owns no timer, exactly as the
    /// wheel accelerator and the double-click window require.
    pub fn handle_alt_viewport_input(&mut self, data: &str, now_ms: u64) -> bool {
        if data == FOCUS_OUT || data == FOCUS_IN {
            self.alt_screen_handle_focus(data);
            return true;
        }
        // pi tries the wheel first (`:696`), because a wheel report is also a
        // well-formed SGR report and would otherwise be read as a press.
        if let Some(wheel) = crate::mouse::parse_wheel_event(data) {
            return self.handle_alt_wheel_input(&wheel, now_ms);
        }
        if let Some(raw) = crate::mouse::parse_sgr_mouse_event(data) {
            let frame = self.current_layout_frame();
            return self.handle_alt_mouse_event(&frame, &raw, now_ms) == MouseOutcome::Consumed;
        }
        if crate::mouse::is_mouse_sequence(data) {
            return true;
        }
        self.handle_alt_viewport_key(data)
    }

    /// The wheel branch of `handleViewportInput`, `:696-712`: overlay first,
    /// then the layout tree, then the scroll views — with the defer rule in
    /// between, which is what keeps a focused overlay's wheel to itself.
    fn handle_alt_wheel_input(&mut self, wheel: &WheelEvent, now_ms: u64) -> bool {
        let wheel_delta = self.alt_screen_wheel_delta(wheel, now_ms as f64);
        let raw = SgrMouseEvent {
            button: wheel.button as u32,
            x: wheel.x,
            y: wheel.y,
            release: false,
        };
        let event = self
            .create_mouse_event(&raw, Some(MouseEventKind::Wheel), None)
            .with_wheel_delta(wheel_delta);
        let overlay = self.dispatch_mouse_to_overlay(&event);
        let result = match overlay.result {
            Some(result) => Some(result),
            None if overlay.hit => None,
            None => {
                let frame = self.current_layout_frame();
                self.alt_screen_dispatch_mouse_to_layout(&frame, &event)
            }
        };
        if let Some(result) = result {
            if self.apply_alt_mouse_dispatch_result(&event, &result) {
                self.request_render();
            }
            return true;
        }
        if self.should_defer_viewport_input_to_overlay() {
            return false;
        }
        let frame = self.current_layout_frame();
        self.alt_screen_route_wheel(&frame, wheel.x, wheel.y, wheel_delta);
        true
    }

    /// The key half of `handleViewportInput`, `:722-786`.
    ///
    /// The search keys come first, then the defer rule, then the viewport
    /// keys — pi's order, and the reason `pageUp` scrolls the transcript in
    /// fullscreen while it moves the editor cursor in regular mode.
    // Long because it mirrors pi's key dispatch (`tui-alt-screen.ts:671-786`).
    #[allow(clippy::too_many_lines)]
    fn handle_alt_viewport_key(&mut self, data: &str) -> bool {
        use crate::keybindings::{action, matches};
        let is_release = crate::keys::is_key_release(data);

        if matches(data, action::ALT_SCREEN_SEARCH) {
            if !is_release {
                self.toggle_search();
            }
            return true;
        }
        if self.search_overlay_is_focused() {
            if matches(data, action::ALT_SCREEN_SEARCH_NEXT) {
                if !is_release {
                    self.navigate_search(1);
                }
                return true;
            }
            if matches(data, action::ALT_SCREEN_SEARCH_PREVIOUS) {
                if !is_release {
                    self.navigate_search(-1);
                }
                return true;
            }
            if matches(data, action::ALT_SCREEN_SEARCH_CLOSE) {
                if !is_release {
                    self.close_search();
                }
                return true;
            }
        }
        if self.should_defer_viewport_input_to_overlay() {
            return false;
        }
        if matches(data, action::ALT_SCREEN_PAGE_UP) {
            if !is_release {
                let page = self
                    .primary_viewport_height()
                    .saturating_sub(PAGE_SCROLL_OVERLAP)
                    .max(1);
                self.alt_screen_scroll_by(-(page as i64));
            }
            return true;
        }
        if matches(data, action::ALT_SCREEN_PAGE_DOWN) {
            if !is_release {
                let page = self
                    .primary_viewport_height()
                    .saturating_sub(PAGE_SCROLL_OVERLAP)
                    .max(1);
                self.alt_screen_scroll_by(page as i64);
            }
            return true;
        }
        if matches(data, action::ALT_SCREEN_HALF_PAGE_UP) {
            if !is_release {
                let half = (self.primary_viewport_height() / 2).max(1);
                self.alt_screen_scroll_by(-(half as i64));
            }
            return true;
        }
        if matches(data, action::ALT_SCREEN_HALF_PAGE_DOWN) {
            if !is_release {
                let half = (self.primary_viewport_height() / 2).max(1);
                self.alt_screen_scroll_by(half as i64);
            }
            return true;
        }
        if matches(data, action::ALT_SCREEN_LINE_UP) {
            if !is_release {
                self.alt_screen_scroll_by(-1);
            }
            return true;
        }
        if matches(data, action::ALT_SCREEN_LINE_DOWN) {
            if !is_release {
                self.alt_screen_scroll_by(1);
            }
            return true;
        }
        if matches(data, action::ALT_SCREEN_PREVIOUS_PROMPT) {
            if !is_release {
                self.alt_screen_scroll_to_prompt(-1);
            }
            return true;
        }
        if matches(data, action::ALT_SCREEN_NEXT_PROMPT) {
            if !is_release {
                self.alt_screen_scroll_to_prompt(1);
            }
            return true;
        }
        if matches(data, action::ALT_SCREEN_TOP) {
            if !is_release {
                self.alt_screen_scroll_to_top();
            }
            return true;
        }
        if matches(data, action::ALT_SCREEN_BOTTOM) {
            if !is_release {
                self.alt_screen_scroll_to_bottom();
            }
            return true;
        }
        false
    }

    /// `shouldDeferViewportInputToOverlay`, `:665-667`: a focused overlay owns
    /// the viewport keys, unless the search overlay itself holds focus — it
    /// is the one overlay the viewport shares input with.
    fn should_defer_viewport_input_to_overlay(&self) -> bool {
        self.is_overlay_focused() && !self.search_overlay_is_focused()
    }

    fn is_overlay_focused(&self) -> bool {
        matches!(
            self.focused_target(),
            Some(crate::tui::FocusTarget::Overlay(_))
        )
    }

    /// The search overlay's own `isFocused()`, `:530`.
    fn search_overlay_is_focused(&self) -> bool {
        self.alt()
            .and_then(|alt| alt.active_search.as_ref().map(|search| search.overlay))
            .is_some_and(|overlay| {
                self.focused_target() == Some(crate::tui::FocusTarget::Overlay(overlay))
            })
    }

    /// The frame the viewport acts on. pi reads `currentLayout`, painted by the
    /// last render; this port re-measures when the frame is stale so the hit
    /// tests cannot resolve against a layout the terminal no longer has.
    fn current_layout_frame(&mut self) -> LayoutFrame {
        let (width, height) = self.terminal_size();
        self.alt_screen_layout_frame(width, height)
    }

    // -------------------------------------------------------------- render

    /// `doRender`, `:1671-1755`.
    ///
    /// The whole alt-screen frame in pi's order: lay the frame out, refresh the
    /// search (and lay it out again when the view moved), strip the shell's
    /// prompt markers, paint the search highlights, the jump-to-end label, the
    /// overlays, the selection and the flashes, then write only the rows that
    /// changed. The main-screen renderer never runs while the alt screen is
    /// active — [`crate::tui::Tui::render_now`] dispatches here instead.
    // Render loop parity with upstream `doRender`; splitting it would lose the
    // one-to-one mapping with the TS source.
    #[allow(clippy::too_many_lines)]
    pub fn alt_screen_do_render(&mut self) {
        if self.is_stopped() || !self.alt_screen_active() {
            return;
        }
        let (columns, rows) = self.terminal_size();
        let width = columns.max(1);
        let height = rows.max(1) as i64;

        let mut layout = self.alt_screen_layout_frame(width, rows);
        if self
            .refresh_search(&layout)
            .is_some_and(|(_, revealed)| revealed)
        {
            layout = self.alt_screen_layout_frame(width, rows);
        }
        let mut screen: Vec<String> = layout
            .lines
            .iter()
            .map(|line| strip_osc133_zone_prefix(line))
            .collect();
        screen = self.alt_screen_apply_search_highlights(&screen, &layout);
        screen = self.composite_scroll_to_end_indicator(screen, &layout, width);
        screen = self.composite_overlays(screen, width, height);
        if screen.len() > height as usize {
            screen = screen.split_off(screen.len() - height as usize);
        }
        screen = self.alt_screen_apply_selection(&screen, &layout);
        screen = self.composite_flashes(screen, width, height);

        // `extractCursorPosition`, `tui-main-screen.ts:1442-1460`: before the
        // resets are appended, and before the frame is truncated to width.
        let cursor = Tui::extract_cursor_position(&mut screen, height);

        let screen: Vec<String> = crate::tui::apply_line_resets(screen)
            .into_iter()
            .map(|line| {
                if crate::terminal_image::is_image_line(&line)
                    || crate::ansi::visible_width(&line) <= width
                {
                    line
                } else {
                    crate::ansi::slice_by_column(&line, 0, width, true)
                }
            })
            .collect();

        let previous = self
            .alt()
            .map(|alt| alt.previous_screen.clone())
            .unwrap_or_default();
        let previous_width = self.alt().map_or(0, |alt| alt.previous_screen_width);
        let previous_height = self.alt().map_or(0, |alt| alt.previous_screen_height);
        let full_redraw =
            previous.is_empty() || previous_width != width || previous_height != height as usize;
        let images_need_redraw = screen.iter().enumerate().any(|(row, line)| {
            line != previous.get(row).map_or("", String::as_str)
                && (crate::terminal_image::is_image_line(line)
                    || crate::terminal_image::is_image_line(
                        previous.get(row).map_or("", String::as_str),
                    ))
        });
        let redraw_images = full_redraw || images_need_redraw;
        let protocol = self
            .alt()
            .map_or(crate::terminal_image::ImageProtocol::Disabled, |alt| {
                alt.image_protocol
            });
        let had_uploaded_kitty_images = self
            .alt()
            .is_some_and(|alt| !alt.uploaded_kitty_images.is_empty())
            && protocol == crate::terminal_image::ImageProtocol::Kitty;
        let prepared = if redraw_images && protocol == crate::terminal_image::ImageProtocol::Kitty {
            self.prepare_kitty_screen(screen.clone())
        } else {
            (screen.clone(), String::new())
        };
        let (prepared_lines, evicted_deletion) = prepared;

        let mut buffer = String::from(BEGIN_SYNCHRONIZED_OUTPUT);
        if full_redraw {
            // `:1709`: pi counts the redraw here, so the counter keeps
            // climbing while the alt screen is up.
            self.record_full_redraw();
            buffer.push_str(
                &if protocol == crate::terminal_image::ImageProtocol::Kitty {
                    if had_uploaded_kitty_images {
                        crate::terminal_image::delete_all_kitty_placements()
                    } else {
                        self.delete_kitty_images()
                    }
                } else {
                    self.delete_kitty_images()
                },
            );
            buffer.push_str("\x1b[2J");
        } else if images_need_redraw {
            if protocol == crate::terminal_image::ImageProtocol::Iterm2 {
                buffer.push_str("\x1b[2J");
            } else if protocol == crate::terminal_image::ImageProtocol::Kitty {
                buffer.push_str(&crate::terminal_image::delete_all_kitty_placements());
            }
        }
        buffer.push_str(&evicted_deletion);

        // `:1721-1730`: WezTerm erases a Kitty placement when a later EL
        // clears a row it intersects, so those frames clear every touched row
        // before drawing anything.
        let clear_rows_before_kitty_images = redraw_images
            && protocol == crate::terminal_image::ImageProtocol::Kitty
            && prepared_lines
                .iter()
                .any(|line| crate::terminal_image::is_image_line(line))
            && Tui::is_wezterm();
        if clear_rows_before_kitty_images {
            for row in 0..height {
                if !full_redraw
                    && !images_need_redraw
                    && prepared_lines[row as usize] == previous[row as usize]
                {
                    continue;
                }
                {
                    use std::fmt::Write as _;
                    let _ = write!(buffer, "\x1b[{};1H\x1b[2K", row + 1);
                }
            }
        }
        for row in 0..height {
            if !full_redraw
                && !images_need_redraw
                && prepared_lines[row as usize] == previous[row as usize]
            {
                continue;
            }
            {
                use std::fmt::Write as _;
                let _ = write!(
                    buffer,
                    "\x1b[{};1H{}{}",
                    row + 1,
                    if clear_rows_before_kitty_images {
                        ""
                    } else {
                        "\x1b[2K"
                    },
                    prepared_lines.get(row as usize).map_or("", String::as_str)
                );
            }
        }
        match cursor {
            Some(cursor) => {
                {
                    use std::fmt::Write as _;
                    let _ = write!(
                        buffer,
                        "\x1b[{};{}H",
                        cursor.row + 1,
                        (width as i64).min(cursor.col as i64) + 1
                    );
                }
                buffer.push_str(if self.hardware_cursor_shown() {
                    "\x1b[?25h"
                } else {
                    "\x1b[?25l"
                });
            }
            None => buffer.push_str("\x1b[?25l"),
        }
        buffer.push_str(END_SYNCHRONIZED_OUTPUT);
        self.write_terminal(&buffer);

        if let Some(alt) = self.alt_screen.as_mut() {
            alt.previous_screen = screen;
            alt.previous_screen_width = width;
            alt.previous_screen_height = height as usize;
        }
    }

    /// `compositeScrollToEndIndicator`, `:1633-1656`.
    ///
    /// The label sits on the last row of the primary clip, centred, and never
    /// over the scrollbar. It is drawn only while a follow-end view is scrolled
    /// away from its end, which is the whole point of the hint.
    fn composite_scroll_to_end_indicator(
        &mut self,
        mut screen: Vec<String>,
        layout: &LayoutFrame,
        width: usize,
    ) -> Vec<String> {
        self.set_scroll_to_end_indicator_rect(None);
        let Some(scroll_view) = layout.primary_scroll_view.or_else(|| {
            self.alt()
                .and_then(|alt| alt.implicit_root.as_ref())
                .map(|root| crate::component::component_id(root.as_ref()))
        }) else {
            return screen;
        };
        let Some(label) = self.scroll_to_end_indicator_label() else {
            return screen;
        };
        let Some(box_) = get_scroll_view_box(layout, scroll_view) else {
            return screen;
        };
        // `:1640`: the label only exists while a follow-end view is scrolled
        // away from its end.
        let following_end = self
            .scroll_view_state(scroll_view)
            .is_some_and(|_| self.scroll_view_is_following_end(scroll_view));
        if following_end {
            return screen;
        }
        let clip = box_.clip;
        if clip.width <= 0 || clip.height <= 0 {
            return screen;
        }
        let row = clip.y + clip.height - 1;
        if row < 0 || row as usize >= screen.len() {
            return screen;
        }
        if crate::terminal_image::is_image_line(&screen[row as usize]) {
            return screen;
        }
        let scrollbar_column =
            get_scrollbar_geometry(box_, false).map(|geometry| geometry.column as i64);
        let label = crate::ansi::truncate_to_width(&label, clip.width.max(0) as usize, "", false);
        let label_width = crate::ansi::visible_width(&label) as i64;
        if label_width == 0 {
            return screen;
        }
        let column = clip.x + (clip.width - label_width) / 2;
        let right_edge = scrollbar_column.unwrap_or(clip.x + clip.width);
        let available = (right_edge - column).max(0) as usize;
        let text = crate::ansi::truncate_to_width(&label, available, "", false);
        let text_width = crate::ansi::visible_width(&text);
        if text_width == 0 {
            return screen;
        }
        let composited = {
            let line = screen[row as usize].clone();
            crate::component::composite_tui_line(
                &line,
                &text,
                column.max(0) as usize,
                text_width,
                width,
            )
        };
        screen[row as usize] = composited;
        self.set_scroll_to_end_indicator_rect(Some(ScrollToEndIndicatorRect {
            row,
            column,
            width: text_width as i64,
        }));
        screen
    }

    /// `compositeFlashes`, `:1657-1669`: the newest flash lands on the last
    /// row, right-aligned, over whatever is there.
    fn composite_flashes(
        &mut self,
        mut screen: Vec<String>,
        width: usize,
        height: i64,
    ) -> Vec<String> {
        let mut flash_lines = self
            .alt()
            .map(|alt| alt.flashes.render(width))
            .unwrap_or_default();
        if flash_lines.is_empty() {
            return screen;
        }
        let flash_lines =
            flash_lines.split_off(flash_lines.len().saturating_sub(height.max(0) as usize));
        while screen.len() < height.max(0) as usize {
            screen.push(String::new());
        }
        for (row, line) in flash_lines.iter().enumerate() {
            let line_width = crate::ansi::visible_width(line);
            if line_width == 0 {
                continue;
            }
            screen[row] = crate::component::composite_tui_line(
                &screen[row],
                line,
                width - line_width,
                line_width,
                width,
            );
        }
        screen
    }

    /// `prepareKittyScreen`, `:425-470`.
    ///
    /// A line whose image was already transmitted becomes its plain replacement
    /// line, so the terminal does not receive the payload twice; images that
    /// scrolled out of view are evicted once the cache budgets are exceeded.
    fn prepare_kitty_screen(&mut self, screen: Vec<String>) -> (Vec<String>, String) {
        let mut visible_ids: Vec<u32> = Vec::new();
        let mut lines = Vec::with_capacity(screen.len());
        for line in screen {
            let Some(placement) = crate::terminal_image::get_kitty_image_placement(&line) else {
                lines.push(line);
                continue;
            };
            visible_ids.push(placement.image_id);
            let next = CachedKittyImage {
                transmission_generation: placement.transmission_generation,
                transmission_bytes: placement.transmission_bytes,
                estimated_decoded_bytes: placement.estimated_decoded_bytes,
            };
            // `:434-439`: the cache is keyed by image id, and every pass
            // rewrites the entry — a delete then a set, which is also how a
            // `Map` refreshes its insertion order. The line is replaced only
            // when the *generation* is unchanged, so a re-transmitted image
            // sends its payload again.
            let cached_generation = self.alt().and_then(|alt| {
                alt.uploaded_kitty_images
                    .iter()
                    .find(|(id, _)| *id == placement.image_id)
                    .map(|(_, image)| image.transmission_generation)
            });
            let replacement = if cached_generation == Some(placement.transmission_generation) {
                placement.replacement_line.clone()
            } else {
                line.clone()
            };
            if let Some(alt) = self.alt_screen.as_mut() {
                alt.uploaded_kitty_images
                    .retain(|(id, _)| *id != placement.image_id);
                alt.uploaded_kitty_images.push((placement.image_id, next));
            }
            lines.push(replacement);
        }
        let mut evicted_deletion = String::new();
        let mut offscreen_count = 0usize;
        let mut offscreen_transmission = 0usize;
        let mut offscreen_decoded = 0u64;
        let Some(alt) = self.alt() else {
            return (lines, evicted_deletion);
        };
        for (image_id, image) in &alt.uploaded_kitty_images {
            if visible_ids.contains(image_id) {
                continue;
            }
            offscreen_count += 1;
            offscreen_transmission += image.transmission_bytes;
            offscreen_decoded += image.estimated_decoded_bytes;
        }
        let mut evict: Vec<u32> = Vec::new();
        {
            let alt = self.alt().expect("checked above");
            for (image_id, image) in &alt.uploaded_kitty_images {
                if offscreen_count <= MAX_CACHED_OFFSCREEN_KITTY_IMAGES
                    && offscreen_transmission <= MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES
                    && offscreen_decoded <= MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES
                {
                    break;
                }
                if visible_ids.contains(image_id) {
                    continue;
                }
                evict.push(*image_id);
                offscreen_count = offscreen_count.saturating_sub(1);
                offscreen_transmission =
                    offscreen_transmission.saturating_sub(image.transmission_bytes);
                offscreen_decoded = offscreen_decoded.saturating_sub(image.estimated_decoded_bytes);
            }
        }
        // `:458-468`: pi deletes while it iterates the map, so the escapes
        // come out in insertion order. Collecting the ids first and then
        // removing them by id keeps that order and the borrow rules.
        if let Some(alt) = self.alt_screen.as_mut() {
            for image_id in evict {
                evicted_deletion.push_str(&crate::terminal_image::delete_kitty_image(image_id));
                alt.uploaded_kitty_images.retain(|(id, _)| *id != image_id);
            }
        }
        (lines, evicted_deletion)
    }

    /// `scrollView.followEnd && scrollView.isFollowingEnd`, `:1640`.
    fn scroll_view_is_following_end(&mut self, scroll_view: ComponentId) -> bool {
        self.scroll_view_mut(scroll_view)
            .is_some_and(|view| view.state.is_following_end())
    }

    /// `process.env.WEZTERM_PANE || process.env.TERM_PROGRAM === "wezterm"`,
    /// `:1726`. Read through the same injected environment the capability
    /// probe uses, so a test never reads the real one.
    fn is_wezterm() -> bool {
        let env = crate::terminal_image::capability_environment();
        env.get("WEZTERM_PANE").is_some()
            || env.get("TERM_PROGRAM").map(str::to_lowercase) == Some("wezterm".to_string())
    }

    /// `deleteKittyImages`, `:421-423`.
    fn delete_kitty_images(&self) -> String {
        if self
            .alt()
            .is_some_and(|alt| alt.image_protocol == crate::terminal_image::ImageProtocol::Kitty)
        {
            crate::terminal_image::delete_all_kitty_images()
        } else {
            String::new()
        }
    }

    // ------------------------------------------------------------- search

    /// `toggleSearch`, `:511-537`.
    ///
    /// The overlay is anchored top-right at 40% with a 32-cell minimum and a
    /// one-cell margin, and the component is built with the navigation
    /// styling hooks pi passes in.
    pub fn toggle_search(&mut self) {
        if self.close_search() {
            return;
        }
        let anchor_row = self.alt_screen_viewport_top();
        let mut options =
            AltScreenSearchComponentOptions::new(std::sync::Arc::new(|_query: &str| {}))
                .with_keybindings();
        options.navigation_button_style = self.alt().map_or_else(
            || -> crate::alt_screen_search::SearchButtonStyle {
                std::sync::Arc::new(|text: &str, _hovered: bool| text.to_string())
            },
            |alt| std::sync::Arc::clone(&alt.search_navigation_button_style),
        );
        let component = AltScreenSearchComponent::new(options);
        let overlay = self.show_overlay(
            Box::new(component),
            Some(crate::tui::OverlayOptions {
                anchor: crate::tui::OverlayAnchor::TopRight,
                width: Some(SizeValue::Percent(40.0)),
                min_width: Some(32),
                margin: OverlayMargin {
                    top: Some(1),
                    right: Some(1),
                    bottom: Some(1),
                    left: Some(1),
                },
                ..Default::default()
            }),
        );
        let alt = self.alt_state();
        alt.active_search = Some(ActiveSearch {
            overlay,
            index: AltScreenSearchIndex::new(),
            query: String::new(),
            matches: Vec::new(),
            selected_index: -1,
            selected_key: None,
            anchor_row,
            selection_mode: SearchSelectionMode::Query,
        });
    }

    /// `closeSearch`, `:538-544`. `true` when a search was open.
    pub fn close_search(&mut self) -> bool {
        let Some(alt) = self.alt_screen.as_mut() else {
            return false;
        };
        let Some(search) = alt.active_search.take() else {
            return false;
        };
        self.hide_overlay(search.overlay);
        self.request_render();
        true
    }

    /// Whether the search overlay is open.
    pub fn search_is_open(&self) -> bool {
        self.alt().is_some_and(|alt| alt.active_search.is_some())
    }

    /// `updateSearchQuery`, `:546-555`.
    ///
    /// The anchor moves to the current match's first row, the result counter
    /// resets to "none of none", and the next refresh picks the first match at
    /// or after it.
    pub fn update_search_query(&mut self, query: &str) {
        let anchor_row = self.alt_screen_viewport_top();
        {
            let Some(alt) = self.alt_screen.as_mut() else {
                return;
            };
            let Some(search) = alt.active_search.as_mut() else {
                return;
            };
            if query == search.query {
                return;
            }
            search.query = query.to_string();
            search.anchor_row = anchor_row;
            search.selection_mode = SearchSelectionMode::Query;
            search.matches.clear();
            search.selected_index = -1;
        }
        self.set_search_result(-1, 0);
        self.request_render();
    }

    /// `navigateSearch`, `:557-563`.
    pub fn navigate_search(&mut self, direction: i64) {
        {
            let Some(alt) = self.alt_screen.as_mut() else {
                return;
            };
            let Some(search) = alt.active_search.as_mut() else {
                return;
            };
            if search.query.is_empty() {
                return;
            }
            search.selection_mode = if direction < 0 {
                SearchSelectionMode::Previous
            } else {
                SearchSelectionMode::Next
            };
        }
        self.request_render();
    }

    /// `refreshSearch`, `:586-655`.
    ///
    /// Returns the selected match index and whether the view moved — pi's
    /// return value, which the renderer reads as "lay the frame out again"
    /// (`:1670-1672`). `None` when there is no search to refresh.
    // Long for parity with pi's search refresh (`tui-alt-screen.ts:1600-1700`).
    #[allow(clippy::too_many_lines)]
    pub fn refresh_search(&mut self, layout: &LayoutFrame) -> Option<(i64, bool)> {
        let scroll_view = layout.primary_scroll_view?;
        let box_ = get_scroll_view_box(layout, scroll_view)?;
        let lines = box_.scroll_content_lines.clone()?;
        let alt = self.alt_screen.as_mut()?;
        let search = alt.active_search.as_mut()?;
        if search.query.trim().is_empty() {
            search.matches.clear();
            search.selected_index = -1;
            search.selected_key = None;
            search.selection_mode = SearchSelectionMode::Retain;
            self.set_search_result(-1, 0);
            return Some((-1, false));
        }
        let should_reveal_selection = search.selection_mode != SearchSelectionMode::Retain;
        let result = search.index.search(&lines, &search.query);
        let matches = result.matches;
        let count = matches.len();
        search.matches = matches;
        let matches = &search.matches;
        if !result.changed && search.selection_mode == SearchSelectionMode::Retain {
            return Some((search.selected_index, false));
        }
        let exact_index = if result.changed {
            match &search.selected_key {
                Some(key) => matches
                    .iter()
                    .position(|found| {
                        crate::alt_screen_search::get_alt_screen_search_match_key(found) == *key
                    })
                    .map_or(-1, |index| index as i64),
                None => -1,
            }
        } else {
            search.selected_index
        };
        let selected = if count == 0 {
            -1
        } else {
            let last = count as i64 - 1;
            match search.selection_mode {
                // Binary search for the first match at or after the anchor row.
                SearchSelectionMode::Query => {
                    let rows: Vec<usize> = matches
                        .iter()
                        .map(|found| found.segments.first().map_or(0, |segment| segment.row))
                        .collect();
                    let mut low = 0usize;
                    let mut high = count;
                    while low < high {
                        let middle = low + (high - low) / 2;
                        if rows[middle] < search.anchor_row {
                            low = middle + 1;
                        } else {
                            high = middle;
                        }
                    }
                    if low < count { low as i64 } else { 0 }
                }
                SearchSelectionMode::Next => {
                    let base = if exact_index >= 0 {
                        exact_index
                    } else {
                        search.selected_index.min(last)
                    };
                    if base < 0 {
                        0
                    } else {
                        (base + 1) % count as i64
                    }
                }
                SearchSelectionMode::Previous => {
                    let base = if exact_index >= 0 {
                        exact_index
                    } else {
                        search.selected_index.min(last)
                    };
                    if base < 0 {
                        last
                    } else {
                        (base - 1 + count as i64) % count as i64
                    }
                }
                SearchSelectionMode::Retain => {
                    if exact_index >= 0 {
                        exact_index
                    } else {
                        search.selected_index.clamp(0, last)
                    }
                }
            }
        };
        search.selected_index = selected;
        search.selected_key = matches
            .get(selected.max(0) as usize)
            .map(crate::alt_screen_search::get_alt_screen_search_match_key);
        search.selection_mode = SearchSelectionMode::Retain;
        self.set_search_result(selected, count);
        // `revealSelection`, `:637-653`: a match outside the viewport scrolls
        // the view so its first segment sits a third of a screen down. This
        // only runs for an explicit navigation, never for a typed query.
        let revealed = if should_reveal_selection {
            self.reveal_search_selection(scroll_view, selected)
        } else {
            false
        };
        Some((selected, revealed))
    }

    /// `revealSelection`, `:637-653`. `false` when there is nothing to reveal.
    fn reveal_search_selection(&mut self, scroll_view: ComponentId, selected: i64) -> bool {
        let Some(segments) = self
            .alt()
            .and_then(|alt| alt.active_search.as_ref())
            .and_then(|search| search.matches.get(selected.max(0) as usize))
            .map(|found| found.segments.clone())
        else {
            return false;
        };
        let (Some(first), Some(last)) = (segments.first(), segments.last()) else {
            return false;
        };
        let Some(view) = self.scroll_view_mut(scroll_view) else {
            return false;
        };
        if view.state.viewport_height() == 0 {
            return false;
        }
        let before = view.state.scroll_top();
        let visible_bottom = before + view.state.viewport_height() - 1;
        let mut target = before;
        if first.row < before || last.row > visible_bottom {
            target = first.row.saturating_sub(view.state.viewport_height() / 3);
        }
        view.scroll_to(target);
        view.state.scroll_top() != before
    }

    fn set_search_result(&mut self, index: i64, count: usize) {
        let Some(alt) = self.alt_screen.as_mut() else {
            return;
        };
        let Some(overlay) = alt.active_search.as_ref().map(|search| search.overlay) else {
            return;
        };
        if let Some(component) = self.search_component_mut(overlay) {
            component.set_result(index, count);
        }
    }

    /// The search component inside its overlay. It is reached by downcast for
    /// the same reason a `ScrollView` is: pi holds the typed reference
    /// (`tui-alt-screen.ts:512-517`) and the overlay owns it by value here.
    fn search_component_mut(
        &mut self,
        overlay: OverlayId,
    ) -> Option<&mut crate::alt_screen_search::AltScreenSearchComponent> {
        self.overlay_component_mut(overlay)?
            .as_any_mut()?
            .downcast_mut::<crate::alt_screen_search::AltScreenSearchComponent>()
    }

    /// The search overlay's last rendered bounds, if it is open.
    pub fn search_overlay_bounds(&self) -> Option<OverlayBounds> {
        let alt = self.alt()?;
        let search = alt.active_search.as_ref()?;
        self.overlay_bounds(search.overlay)
    }

    /// `handleSearchMouseEvent`, `:574-584`.
    pub fn handle_search_mouse_event(&mut self, raw: &SgrMouseEvent) -> bool {
        let Some(bounds) = self.search_overlay_bounds() else {
            return false;
        };
        let direction = self.search_navigation_direction_at(raw.x, raw.y, bounds);
        if let Some(overlay) = self
            .alt()
            .and_then(|alt| alt.active_search.as_ref())
            .map(|s| s.overlay)
        {
            let hovered = self
                .search_component_mut(overlay)
                .is_some_and(|component| component.set_hovered_navigation_direction(direction));
            if hovered {
                self.request_render();
            }
        }
        if direction.is_none() || raw.release || raw.button & 32 != 0 || raw.button & 3 != 0 {
            return false;
        }
        self.navigate_search(if direction == Some(SEARCH_DIRECTION_PREVIOUS) {
            -1
        } else {
            1
        });
        true
    }

    /// `getSearchNavigationDirectionAt`, `:564-572`.
    fn search_navigation_direction_at(
        &mut self,
        x: i64,
        y: i64,
        bounds: OverlayBounds,
    ) -> Option<SearchNavigationDirection> {
        if x < bounds.col
            || x >= bounds.col + bounds.width as i64
            || y < bounds.row
            || y >= bounds.row + bounds.height as i64
        {
            return None;
        }
        let overlay = self.alt()?.active_search.as_ref()?.overlay;
        let component = self.search_component_mut(overlay)?;
        component.get_navigation_direction_at((y - bounds.row) as usize, x - bounds.col)
    }

    // ------------------------------------------------------ focus in/out

    /// `handleViewportInput`'s focus branches, `:672-684`.
    ///
    /// Focus-out releases every gesture: a half-finished press, a scrollbar
    /// drag, a scrollbar hover and any text selection. It reports whether the
    /// caller should repaint, which is pi's `requestRender` on the way out.
    pub fn alt_screen_handle_focus(&mut self, data: &str) -> bool {
        if data == FOCUS_IN {
            return true;
        }
        if data != FOCUS_OUT {
            return false;
        }
        let had_selection = self.alt().is_some_and(|alt| alt.selection_press_active);
        let had_non_empty_selection = had_selection && self.selection_bounds().is_some();
        self.alt_state().selection_press_active = false;
        self.alt_state().last_click = None;
        self.alt_state().selection_dragged = false;
        self.stop_scrollbar_hover();
        self.stop_scrollbar_drag();
        self.clear_alt_mouse_gesture();
        self.clear_alt_text_selection();
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.last_component_click.reset();
        }
        had_non_empty_selection
    }

    // ------------------------------------------------- selection plumbing

    /// `clearTextSelection`, `:882-891`.
    ///
    /// Note what pi does *not* clear: `lastClick` and `lastComponentClick`
    /// survive, so a selection gesture never breaks the click chains.
    pub fn clear_alt_text_selection(&mut self) {
        self.stop_selection_auto_scroll();
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.selection_press_active = false;
            alt.selection_anchor = None;
            alt.selection_focus = None;
            alt.selection_granularity = SelectionGranularity::Character;
            alt.selection_initial_range = None;
            alt.pressed_url = None;
            alt.selection_dragged = false;
            alt.selection_text = None;
            alt.copy_requested = false;
        }
    }

    /// The active selection's text. `getActiveSelectionText`, `:1431`.
    pub fn alt_screen_active_selection_text(&self) -> Option<&str> {
        self.alt().and_then(|alt| alt.selection_text.as_deref())
    }

    /// `hasActiveSelection`, `:304-307`.
    pub fn alt_screen_has_active_selection(&self) -> bool {
        self.alt_screen_active_selection_text()
            .is_some_and(|text| !text.is_empty())
    }

    /// Drain the copy-on-select request. `pi-tui` owns no clipboard, so the
    /// host performs the copy with
    /// [`Tui::alt_screen_active_selection_text`].
    pub fn take_alt_screen_copy_request(&mut self) -> bool {
        let requested = self
            .alt()
            .is_some_and(|alt| alt.copy_on_select && alt.copy_requested);
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.copy_requested = false;
        }
        requested
    }

    /// Record a completed selection, so a host can read and copy it. The
    /// renderer that produces it lands with the selection port.
    pub fn set_alt_screen_selection_text(&mut self, text: Option<String>) {
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.copy_requested = text.is_some();
            alt.selection_text = text;
        }
        self.request_render();
    }

    /// `applySelectionHighlight`, `:1563-1578`.
    ///
    /// Reverse video over the selected cells, re-asserted after every SGR so a
    /// colour change inside the span cannot cancel it.
    pub fn alt_screen_selection_highlight(text: &str) -> String {
        let mut result = String::with_capacity(text.len() + 8);
        result.push_str("\x1b[7m");
        let mut index = 0usize;
        while index < text.len() {
            match crate::ansi::extract_ansi_code(text, index) {
                None => {
                    let ch = text[index..].chars().next().expect("in bounds");
                    result.push(ch);
                    index += ch.len_utf8();
                }
                Some(code) => {
                    result.push_str(code);
                    if code.ends_with('m') {
                        result.push_str("\x1b[7m");
                    }
                    index += code.len();
                }
            }
        }
        result.push_str("\x1b[27m");
        result
    }

    /// `applySelection`, `:1580-1629`.
    ///
    /// A pure function over the painted frame: the host calls it in pi's paint
    /// order (after overlays, before flashes). Inside a scroll view the
    /// content-relative selection is mapped back to screen rows and clipped to
    /// the view, and Kitty image lines are left alone.
    pub fn alt_screen_apply_selection(
        &self,
        screen: &[String],
        layout: &LayoutFrame,
    ) -> Vec<String> {
        let Some((start, end)) = self.selection_bounds() else {
            return screen.to_vec();
        };
        let (columns, _) = self.terminal_size();
        let mut selection = (start, end);
        let mut min_row = 0i64;
        let mut max_row = screen.len() as i64 - 1;
        let mut min_column = 0i64;
        let mut max_column = columns as i64;
        if let Some(scroll_view) = start.scroll_view {
            let Some(box_) = get_scroll_view_box(layout, scroll_view) else {
                return screen.to_vec();
            };
            let Some((visible_top, visible_bottom)) =
                self.scroll_view_visible_rows(layout, scroll_view)
            else {
                return screen.to_vec();
            };
            min_row = visible_top;
            max_row = visible_bottom.min(screen.len() as i64 - 1);
            min_column = 0.max(box_.rect.x).max(box_.clip.x);
            max_column = (columns as i64)
                .min(box_.rect.x + box_.rect.width)
                .min(box_.clip.x + box_.clip.width);
            let scroll_top = box_
                .scroll
                .as_ref()
                .map_or(0, |scroll| scroll.scroll_top as i64);
            selection = (
                SelectionPoint {
                    row: box_.rect.y + start.row - scroll_top,
                    col: box_.rect.x + start.col,
                    ..start
                },
                SelectionPoint {
                    row: box_.rect.y + end.row - scroll_top,
                    col: box_.rect.x + end.col,
                    ..end
                },
            );
        }
        screen
            .iter()
            .enumerate()
            .map(|(row, line)| {
                let row = row as i64;
                if row < min_row
                    || row > max_row
                    || row < selection.0.row
                    || row > selection.1.row
                    || crate::terminal_image::is_image_line(line)
                {
                    return line.clone();
                }
                let line_width = crate::ansi::visible_width(line);
                let (from, to) =
                    Self::selection_columns(line, row, &selection, min_column, max_column);
                if to <= from {
                    return line.clone();
                }
                let before = crate::ansi::slice_by_column(line, 0, from, true);
                let selected = crate::ansi::slice_by_column(line, from, to - from, true);
                let after =
                    crate::ansi::slice_by_column(line, to, line_width.saturating_sub(to), true);
                format!(
                    "{before}{}{after}",
                    Self::alt_screen_selection_highlight(&selected)
                )
            })
            .collect()
    }

    /// `applySearchTextHighlight`, `:1482-1502`.
    ///
    /// The style is re-applied to every plain run between escape codes, so a
    /// colour inside the matched span cannot swallow the highlight — which is
    /// why this is not the selection's re-assert-everything inverse video.
    fn apply_search_text_highlight(text: &str, style: &SearchTextStyle) -> String {
        let mut result = String::with_capacity(text.len());
        let mut plain_start = 0usize;
        let mut index = 0usize;
        while index < text.len() {
            let Some(code) = crate::ansi::extract_ansi_code(text, index) else {
                index += 1;
                continue;
            };
            if index > plain_start {
                result.push_str(&style(&text[plain_start..index]));
            }
            result.push_str(code);
            index += code.len();
            plain_start = index;
        }
        if plain_start < text.len() {
            result.push_str(&style(&text[plain_start..]));
        }
        result
    }

    /// `applySearchHighlights`, `:1504-1561`.
    ///
    /// A pure function over the painted frame: only the rows the primary scroll
    /// view actually shows are touched, and the runs are applied right to left
    /// so an earlier slice never shifts the columns of a later one. The search
    /// hits are content-relative, so a row is `box.rect.y + segment.row -
    /// scrollTop` — the same mapping [`Self::alt_screen_apply_selection`]
    /// uses.
    // Long for parity with pi's highlight pass (`tui-alt-screen.ts:1990-2100`).
    #[allow(clippy::too_many_lines)]
    pub fn alt_screen_apply_search_highlights(
        &self,
        screen: &[String],
        layout: &LayoutFrame,
    ) -> Vec<String> {
        let Some(alt) = self.alt() else {
            return screen.to_vec();
        };
        let Some(search) = alt.active_search.as_ref() else {
            return screen.to_vec();
        };
        if search.selected_index < 0 || search.matches.is_empty() {
            return screen.to_vec();
        }
        let Some(scroll_view) = layout.primary_scroll_view.or_else(|| {
            alt.implicit_root
                .as_ref()
                .map(|root| crate::component::component_id(root.as_ref()))
        }) else {
            return screen.to_vec();
        };
        let Some(box_) = get_scroll_view_box(layout, scroll_view) else {
            return screen.to_vec();
        };
        let scroll_top = box_.scroll.as_ref().map_or(0, |scroll| scroll.scroll_top);
        let (columns, _) = self.terminal_size();

        // `:1516-1525`: the visible window, clipped to the view and, when the
        // scrollbar is drawn, to the left of it.
        let scrollbar_column =
            get_scrollbar_geometry(box_, false).map(|geometry| geometry.column as i64);
        let min_row = 0.max(box_.rect.y).max(box_.clip.y);
        let max_row = (screen.len() as i64)
            .min(box_.rect.y + box_.rect.height)
            .min(box_.clip.y + box_.clip.height);
        let min_column = 0.max(box_.rect.x).max(box_.clip.x);
        let max_column = (columns as i64)
            .min(box_.rect.x + box_.rect.width)
            .min(box_.clip.x + box_.clip.width)
            .min(scrollbar_column.unwrap_or(i64::MAX));
        let min_content_row = scroll_top as i64 + min_row - box_.rect.y;
        let max_content_row = scroll_top as i64 + max_row - box_.rect.y - 1;

        // `:1527-1535`: the first match that can reach the window. Matches are
        // sorted by row, so this is a binary search over the last segment.
        let last_rows: Vec<i64> = search
            .matches
            .iter()
            .map(|found| {
                found
                    .segments
                    .last()
                    .map(|segment| segment.row)
                    .map_or(-1, |row| row as i64)
            })
            .collect();
        let mut low = 0usize;
        let mut high = search.matches.len();
        while low < high {
            let middle = low + (high - low) / 2;
            if last_rows[middle] < min_content_row {
                low = middle + 1;
            } else {
                high = middle;
            }
        }

        let mut ranges_by_row: Vec<(usize, Vec<SearchHighlightRange>)> = Vec::new();
        let selected = search.selected_index as usize;
        for (match_index, found) in search.matches.iter().enumerate().skip(low) {
            let first_row = found.segments.first().map_or(0, |segment| segment.row) as i64;
            if first_row > max_content_row {
                break;
            }
            for segment in &found.segments {
                let row = box_.rect.y + segment.row as i64 - scroll_top as i64;
                if row < min_row || row >= max_row {
                    continue;
                }
                let start_col = min_column.max(box_.rect.x + segment.start_col as i64);
                let end_col = max_column.min(box_.rect.x + segment.end_col as i64);
                if end_col <= start_col {
                    continue;
                }
                let row = row as usize;
                match ranges_by_row
                    .iter_mut()
                    .find(|(existing, _)| *existing == row)
                {
                    Some((_, ranges)) => {
                        ranges.push(SearchHighlightRange {
                            start_col: start_col as usize,
                            end_col: end_col as usize,
                            current: match_index == selected,
                        });
                    }
                    None => ranges_by_row.push((
                        row,
                        vec![SearchHighlightRange {
                            start_col: start_col as usize,
                            end_col: end_col as usize,
                            current: match_index == selected,
                        }],
                    )),
                }
            }
        }

        let mut result = screen.to_vec();
        for (row, mut ranges) in ranges_by_row {
            let Some(line) = result.get(row).cloned() else {
                continue;
            };
            if crate::terminal_image::is_image_line(&line) {
                continue;
            }
            let line_width = crate::ansi::visible_width(&line);
            // Right to left, so slicing does not move the columns still to come.
            ranges.sort_by_key(|range| std::cmp::Reverse(range.start_col));
            let mut line = line;
            for range in ranges {
                let start_col = range.start_col.min(line_width);
                let end_col = range.end_col.min(line_width);
                if end_col <= start_col {
                    continue;
                }
                let before = crate::ansi::slice_by_column(&line, 0, start_col, true);
                let highlighted =
                    crate::ansi::slice_by_column(&line, start_col, end_col - start_col, true);
                let after = crate::ansi::slice_by_column(
                    &line,
                    end_col,
                    line_width.saturating_sub(end_col),
                    true,
                );
                let style = if range.current {
                    &alt.search_current_match_style
                } else {
                    &alt.search_match_style
                };
                line = format!(
                    "{before}{}{after}",
                    Self::apply_search_text_highlight(&highlighted, style)
                );
            }
            result[row] = line;
        }
        result
    }

    /// Record the painted frame, which is what a selection reads and copies
    /// (`previousScreen`, `:316`). The host calls this from its paint step.
    pub fn record_alt_screen_frame(&mut self, screen: Vec<String>) {
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.previous_screen = screen;
        }
    }

    /// The lines of the last painted frame. `getScreenLines`, `:315-317`.
    pub fn alt_screen_lines(&self) -> &[String] {
        self.alt().map_or(&[], |alt| alt.previous_screen.as_slice())
    }

    /// Show a transient message in the alternate-screen flash stack. `flash`,
    /// `:654-656`, and `AltScreenFlashContainer.flash`, `:22-38`.
    ///
    /// `duration_ms` is pi's optional argument (`DEFAULT_FLASH_DURATION_MS`
    /// when absent); `now_ms` is the host clock the deadline is taken from,
    /// because `pi-tui` owns no timer. The frame that expires a flash is
    /// requested by [`Self::expire_alt_screen_flashes`].
    pub fn flash(&mut self, message: impl Into<String>, duration_ms: Option<u64>, now_ms: u64) {
        let duration =
            duration_ms.unwrap_or(crate::components::alt_screen_flash::DEFAULT_FLASH_DURATION_MS);
        self.alt_state().flashes.flash(message, duration, now_ms);
        self.request_render();
    }

    /// Drop every flash whose deadline has passed. `true` when one went away.
    ///
    /// pi arms a `setTimeout` per flash, so this is that timer: the host calls
    /// it from whatever clock it already has — the input path does, through
    /// [`crate::tui::Tui::handle_input_at`].
    pub fn expire_alt_screen_flashes(&mut self, now_ms: u64) -> bool {
        let expired = self.alt_state().flashes.expire(now_ms);
        if expired {
            self.request_render();
        }
        expired
    }

    /// The live flash messages, oldest first, for a host that composites them
    /// itself.
    pub fn alt_screen_flashes(&self) -> Vec<&str> {
        self.alt()
            .map(|alt| alt.flashes.messages())
            .unwrap_or_default()
    }

    /// The scroll-to-end label area from the last composite.
    pub fn scroll_to_end_indicator_rect(&self) -> Option<ScrollToEndIndicatorRect> {
        self.alt().and_then(|alt| alt.scroll_to_end_indicator_rect)
    }

    /// Record the scroll-to-end label area. `compositeScrollToEndIndicator`
    /// (`:1633-1656`) is called by the frame that paints it, which is the
    /// renderer rather than this module.
    pub fn set_scroll_to_end_indicator_rect(&mut self, rect: Option<ScrollToEndIndicatorRect>) {
        if let Some(alt) = self.alt_screen.as_mut() {
            alt.scroll_to_end_indicator_rect = rect;
        }
    }

    /// Whether the scroll-to-end label is configured.
    pub fn has_scroll_to_end_indicator(&self) -> bool {
        self.alt()
            .is_some_and(|alt| alt.scroll_to_end_indicator.is_some())
    }

    /// The configured label, for the renderer to draw.
    pub fn scroll_to_end_indicator_label(&self) -> Option<String> {
        self.alt()
            .and_then(|alt| alt.scroll_to_end_indicator.as_ref())
            .map(|label| label())
    }
}

/// What a scroll view reports back to the alt screen's routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollViewStateSnapshot {
    overscroll: crate::components::scroll_view::Overscroll,
    viewport_height: usize,
    scroll_top: usize,
}

/// A frame with nothing mounted: a single empty leaf box covering the
/// viewport, so hit-testing and the scrollbar have somewhere to look.
fn empty_frame(width: usize, height: usize) -> LayoutFrame {
    let safe_width = width.max(1) as i64;
    let safe_height = height.max(1) as i64;
    let rect = crate::layout::LayoutRect {
        x: 0,
        y: 0,
        width: safe_width,
        height: safe_height,
    };
    LayoutFrame {
        root: crate::layout::LayoutBox {
            rect,
            clip: rect,
            children: Vec::new(),
            lines: Some(Vec::new()),
            line_offset: 0,
            scroll: None,
            layer: 0,
            component: None,
            scroll_view: None,
            scroll_content_lines: None,
        },
        width: safe_width as usize,
        height: safe_height as usize,
        lines: Vec::new(),
        primary_scroll_view: None,
        scroll_updates: Vec::new(),
    }
}

fn contains(bounds: OverlayBounds, x: i64, y: i64) -> bool {
    x >= bounds.col
        && x < bounds.col + bounds.width as i64
        && y >= bounds.row
        && y < bounds.row + bounds.height as i64
}
