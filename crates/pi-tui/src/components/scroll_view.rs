//! `ScrollView`: a fixed viewport over one taller child.
//! `packages/tui/src/components/scroll-view.ts`.
//!
//! The transient auto-scrollbar timer is the one pi behaviour this port does
//! not reproduce: activity is recorded and the host re-renders, but the
//! hide-after-delay step is 4b's, because it needs a timer source. Every
//! scroll, follow-end and layout decision is ported.

use crate::component::Component;
use crate::layout::LayoutNode;

/// Shared render request handle. pi passes a plain closure
/// (`layout.ts:56`); the port needs a shareable handle because the layout
/// pass clones it into every scroll view it lays out.
pub type RenderRequest = std::sync::Arc<dyn Fn() + Send + Sync>;

/// When the scrollbar column is drawn. `scroll-view.ts:7`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scrollbar {
    Hidden,
    Auto,
    Always,
}

/// Whether scrolling past the end hands off to an outer view.
/// `scroll-view.ts:18`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overscroll {
    Chain,
    Contain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollViewOptions {
    pub follow_end: bool,
    pub primary: bool,
    pub overscroll: Overscroll,
    pub scrollbar: Scrollbar,
}

impl Default for ScrollViewOptions {
    fn default() -> Self {
        Self {
            follow_end: false,
            primary: false,
            overscroll: Overscroll::Chain,
            scrollbar: Scrollbar::Hidden,
        }
    }
}

/// Scrollbar painters, kept out of the state so they can be swapped.
#[derive(Clone)]
pub struct ScrollbarStyles {
    pub track: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
    pub thumb: std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>,
}

impl Default for ScrollbarStyles {
    /// `scroll-view.ts:56-57`.
    fn default() -> Self {
        Self {
            track: std::sync::Arc::new(|text: &str| format!("\x1b[90m{text}\x1b[39m")),
            thumb: std::sync::Arc::new(|text: &str| format!("\x1b[37m{text}\x1b[39m")),
        }
    }
}

impl std::fmt::Debug for ScrollbarStyles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScrollbarStyles")
    }
}

/// Options for [`ScrollView::scroll_to`]. `scroll-view.ts:22-25`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScrollToOptions {
    pub disable_follow: bool,
}

/// A pending layout-driven update to a [`ScrollViewState`].
#[derive(Clone)]
pub struct ScrollUpdate {
    pub scroll_top: usize,
    pub state: ScrollViewState,
    pub changed: bool,
}

/// The scrolling state, shared with the layout engine.
// The bools mirror pi's `ScrollViewState` state flags (`scroll-view.ts`) and
// the layout engine reads them directly.
#[allow(clippy::struct_excessive_bools)]
pub struct ScrollViewState {
    current_scroll_top: usize,
    content_height: usize,
    current_viewport_height: usize,
    following_end: bool,
    follow_suppressed_at_end: bool,
    /// The layout pass calls this when scrolling may need a new frame.
    pub request_render: Option<RenderRequest>,
    transient_scrollbar_visible: bool,
    scrollbar_active: bool,
    follow_end: bool,
    primary: bool,
    overscroll: Overscroll,
    scrollbar: Scrollbar,
    pub styles: ScrollbarStyles,
}

impl Clone for ScrollViewState {
    fn clone(&self) -> Self {
        Self {
            current_scroll_top: self.current_scroll_top,
            content_height: self.content_height,
            current_viewport_height: self.current_viewport_height,
            following_end: self.following_end,
            follow_suppressed_at_end: self.follow_suppressed_at_end,
            request_render: self.request_render.clone(),
            transient_scrollbar_visible: self.transient_scrollbar_visible,
            scrollbar_active: self.scrollbar_active,
            follow_end: self.follow_end,
            primary: self.primary,
            overscroll: self.overscroll,
            scrollbar: self.scrollbar,
            styles: self.styles.clone(),
        }
    }
}

impl std::fmt::Debug for ScrollViewState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScrollViewState")
            .field("scroll_top", &self.current_scroll_top)
            .field("content_height", &self.content_height)
            .field("viewport_height", &self.current_viewport_height)
            .finish_non_exhaustive()
    }
}

impl ScrollViewState {
    /// `scroll-view.ts:66-90`.
    pub fn scroll_top(&self) -> usize {
        self.current_scroll_top
    }

    pub fn is_following_end(&self) -> bool {
        self.following_end
    }

    pub fn viewport_height(&self) -> usize {
        self.current_viewport_height
    }

    pub fn content_height(&self) -> usize {
        self.content_height
    }

    pub fn scrollbar(&self) -> Scrollbar {
        self.scrollbar
    }

    pub fn primary(&self) -> bool {
        self.primary
    }

    pub fn overscroll(&self) -> Overscroll {
        self.overscroll
    }

    /// `scroll-view.ts:83-90`.
    pub fn is_scrollbar_visible(&self) -> bool {
        match self.scrollbar {
            Scrollbar::Always => self.current_viewport_height > 0,
            Scrollbar::Auto => {
                self.content_height > self.current_viewport_height
                    && self.transient_scrollbar_visible
            }
            Scrollbar::Hidden => false,
        }
    }

    /// `scroll-view.ts:92-94`.
    pub fn is_scrollbar_active(&self) -> bool {
        self.scrollbar_active
    }

    /// `scroll-view.ts:96-105`.
    pub fn set_scrollbar(&mut self, scrollbar: Scrollbar) {
        if self.scrollbar == scrollbar {
            return;
        }
        self.scrollbar = scrollbar;
        if scrollbar != Scrollbar::Auto {
            self.transient_scrollbar_visible = false;
        } else if self.scrollbar_active {
            self.mark_scrollbar_activity();
        }
        self.request_render();
    }

    /// `scroll-view.ts:113-118`.
    pub fn set_scrollbar_active(&mut self, active: bool) {
        if active == self.scrollbar_active {
            return;
        }
        self.scrollbar_active = active;
        self.mark_scrollbar_activity();
        self.request_render();
    }

    fn mark_scrollbar_activity(&mut self) {
        if self.scrollbar != Scrollbar::Auto || self.content_height <= self.current_viewport_height
        {
            return;
        }
        self.transient_scrollbar_visible = true;
    }

    /// `scroll-view.ts:120-146`.
    pub fn scroll_to(&mut self, scroll_top: usize, options: ScrollToOptions) {
        let max_scroll_top = self
            .content_height
            .saturating_sub(self.current_viewport_height);
        let next = scroll_top.min(max_scroll_top);
        let next_follow_suppressed_at_end = options.disable_follow && next == max_scroll_top;
        let next_following_end =
            !next_follow_suppressed_at_end && self.follow_end && next == max_scroll_top;
        if next == self.current_scroll_top
            && next_following_end == self.following_end
            && next_follow_suppressed_at_end == self.follow_suppressed_at_end
        {
            return;
        }
        let moved = next != self.current_scroll_top;
        self.current_scroll_top = next;
        self.following_end = next_following_end;
        self.follow_suppressed_at_end = next_follow_suppressed_at_end;
        if moved {
            self.mark_scrollbar_activity();
        }
        self.request_render();
    }

    /// `scroll-view.ts:148-167`. Returns the part of the request that could
    /// not be applied, which the host forwards to an outer view when
    /// overscrolling chains.
    pub fn scroll_by(&mut self, lines: i64) -> i64 {
        if lines == 0 {
            return 0;
        }
        let max_scroll_top = self
            .content_height
            .saturating_sub(self.current_viewport_height);
        let start = if self.following_end {
            max_scroll_top
        } else {
            self.current_scroll_top
        };
        let requested = start as i64 + lines;
        let next = requested.clamp(0, max_scroll_top as i64) as usize;
        let moved = next as i64 - start as i64;
        let was_following_end = self.following_end;
        self.current_scroll_top = next;
        self.following_end = self.follow_end && next == max_scroll_top;
        self.follow_suppressed_at_end = false;
        if moved != 0 {
            self.mark_scrollbar_activity();
        }
        if moved != 0 || self.following_end != was_following_end {
            self.request_render();
        }
        lines - moved
    }

    /// `scroll-view.ts:169-180`.
    pub fn scroll_to_start(&mut self) {
        let changed = self.current_scroll_top != 0
            || self.following_end
                != (self.follow_end && self.content_height <= self.current_viewport_height);
        self.current_scroll_top = 0;
        self.following_end = self.follow_end && self.content_height <= self.current_viewport_height;
        self.follow_suppressed_at_end = false;
        if changed {
            self.mark_scrollbar_activity();
            self.request_render();
        }
    }

    /// `scroll-view.ts:182-194`.
    pub fn scroll_to_end(&mut self) {
        let next = self
            .content_height
            .saturating_sub(self.current_viewport_height);
        let changed = self.current_scroll_top != next || self.following_end != self.follow_end;
        self.current_scroll_top = next;
        self.following_end = self.follow_end;
        self.follow_suppressed_at_end = false;
        if changed {
            self.mark_scrollbar_activity();
            self.request_render();
        }
    }

    /// `scroll-view.ts:196-214`. Called by the layout pass every frame.
    pub fn update_layout(&mut self, content_height: usize, viewport_height: usize) {
        let update = self.compute_scroll_update(content_height, viewport_height);
        let changed = update.changed;
        self.apply_scroll_update(update);
        if changed {
            self.request_render();
        }
    }

    /// The pure half of [`ScrollViewState::update_layout`]: the layout engine
    /// measures first and mutates afterwards, because it holds shared
    /// borrows of the components while measuring.
    pub fn compute_scroll_update(
        &self,
        content_height: usize,
        viewport_height: usize,
    ) -> ScrollUpdate {
        let mut next = self.clone();
        next.content_height = content_height;
        next.current_viewport_height = viewport_height;
        let max_scroll_top = content_height.saturating_sub(viewport_height);
        if next.following_end {
            next.current_scroll_top = max_scroll_top;
        } else {
            next.current_scroll_top = next.current_scroll_top.min(max_scroll_top);
        }
        if next.current_scroll_top < max_scroll_top {
            next.follow_suppressed_at_end = false;
        }
        if next.follow_end
            && next.current_scroll_top == max_scroll_top
            && !next.follow_suppressed_at_end
        {
            next.following_end = true;
        }
        if content_height <= viewport_height {
            next.transient_scrollbar_visible = false;
        }
        let scroll_top = next.current_scroll_top;
        ScrollUpdate {
            scroll_top,
            state: next,
            changed: scroll_top != self.current_scroll_top,
        }
    }

    /// Store the layout pass's render request callback. `scroll-view.ts:209`.
    pub fn set_render_request(&mut self, request: Option<RenderRequest>) {
        self.request_render = request;
    }

    /// Apply a [`ScrollUpdate`] produced by
    /// [`ScrollViewState::compute_scroll_update`].
    pub fn apply_scroll_update(&mut self, update: ScrollUpdate) {
        *self = update.state;
    }

    /// `scroll-view.ts:102-104`.
    pub fn content_width(&self, width: usize) -> usize {
        if self.scrollbar == Scrollbar::Always && width > 1 {
            width - 1
        } else {
            width
        }
    }

    fn request_render(&self) {
        if let Some(callback) = &self.request_render {
            callback();
        }
    }
}

/// A viewport over exactly one child. `scroll-view.ts:26-64`.
pub struct ScrollView {
    child: Box<dyn Component>,
    pub state: ScrollViewState,
}

impl ScrollView {
    /// `scroll-view.ts:41-64`.
    pub fn new(child: Box<dyn Component>, options: ScrollViewOptions) -> Self {
        let follow_end = options.follow_end;
        Self {
            child,
            state: ScrollViewState {
                current_scroll_top: 0,
                content_height: 0,
                current_viewport_height: 0,
                following_end: follow_end,
                follow_suppressed_at_end: false,
                request_render: None,
                transient_scrollbar_visible: false,
                scrollbar_active: false,
                follow_end,
                primary: options.primary,
                overscroll: options.overscroll,
                scrollbar: options.scrollbar,
                styles: ScrollbarStyles::default(),
            },
        }
    }

    /// Set the render requester the layout pass uses.
    pub fn set_request_render(&mut self, request: RenderRequest) {
        self.state.request_render = Some(request);
    }

    pub fn scroll_top(&self) -> usize {
        self.state.scroll_top()
    }

    pub fn scroll_to(&mut self, scroll_top: usize) {
        self.state.scroll_to(scroll_top, ScrollToOptions::default());
    }

    pub fn scroll_by(&mut self, lines: i64) -> i64 {
        self.state.scroll_by(lines)
    }

    pub fn scroll_to_end(&mut self) {
        self.state.scroll_to_end();
    }

    pub fn scroll_to_start(&mut self) {
        self.state.scroll_to_start();
    }

    /// `ScrollViewState::update_layout`, for hosts that lay out by hand.
    pub fn update_layout(&mut self, content_height: usize, viewport_height: usize) {
        self.state.update_layout(content_height, viewport_height);
    }

    pub fn child(&self) -> &dyn Component {
        self.child.as_ref()
    }

    pub fn child_mut(&mut self) -> &mut dyn Component {
        self.child.as_mut()
    }

    /// Swap the scrollbar painters. Upstream takes the themed styles as
    /// constructor options (`chat-viewport.js:14-17`); the port builds the
    /// transcript view inside the chat viewport, so they land after.
    pub fn set_styles(&mut self, styles: ScrollbarStyles) {
        self.state.styles = styles;
    }

    /// Give the child back. `implicitScrollView`, `tui-alt-screen.ts:268`: the
    /// alt screen's implicit root is a scroll view it owns for the length of
    /// one frame, wrapping the host's own children.
    pub fn into_child(self) -> Box<dyn Component> {
        self.child
    }
}

impl Component for ScrollView {
    /// The alt screen's wheel routing and scrollbar hold `ScrollView`
    /// references (`tui-alt-screen.ts:988-996`, `:1068-1078`); this is the Rust
    /// equivalent of that structural knowledge.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    /// `scroll-view.ts:216-220`. The scroll offset itself is applied by the
    /// layout pass, so rendering here is the child at its content width.
    fn render(&self, width: usize) -> Vec<String> {
        let content_width = self.state.content_width(width);
        let lines = self.child.render(content_width);
        if content_width == width {
            lines
        } else {
            lines.into_iter().map(|line| format!("{line} ")).collect()
        }
    }

    fn handle_input(&mut self, data: &str) -> Option<crate::component::ComponentMutation> {
        self.child.handle_input(data)
    }

    fn invalidate(&mut self) {
        self.child.invalidate();
    }

    /// `layout-node.ts:48-50`.
    fn layout_node(&self) -> Option<LayoutNode<'_>> {
        Some(scroll_view_node(self))
    }

    fn layout_children_mut(&mut self) -> Vec<&mut crate::component::DynComponent> {
        vec![self.child.as_mut()]
    }

    fn scroll_state_mut(&mut self) -> Option<&mut ScrollViewState> {
        Some(&mut self.state)
    }
}

/// The layout description of this component. `layout-node.ts:36-41`.
pub fn scroll_view_node(view: &ScrollView) -> LayoutNode<'_> {
    LayoutNode::Scroll {
        component: view.child(),
        state: &view.state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::text::Text;

    fn view(lines: usize) -> ScrollView {
        let text = (0..lines)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut view = ScrollView::new(
            Box::new(Text::new(text, 0, 0)),
            ScrollViewOptions {
                follow_end: true,
                scrollbar: Scrollbar::Always,
                ..Default::default()
            },
        );
        view.state.update_layout(lines, 3);
        view
    }

    #[test]
    fn renders_the_child_at_the_content_width() {
        let view = view(3);
        assert_eq!(view.render(8).len(), 3);
        // Scrollbar::Always reserves one column.
        assert_eq!(view.state.content_width(8), 7);
    }

    #[test]
    fn follow_end_keeps_the_viewport_pinned() {
        let mut view = view(10);
        view.state.update_layout(10, 4);
        assert_eq!(view.scroll_top(), 6);
        assert!(view.state.is_following_end());
    }

    #[test]
    fn scroll_by_returns_the_unapplied_remainder() {
        let mut view = view(10);
        view.state.update_layout(10, 4);
        assert_eq!(view.scroll_by(-2), 0);
        assert_eq!(view.scroll_top(), 4);
        assert_eq!(view.scroll_by(-10), -6);
        assert_eq!(view.scroll_top(), 0);
        assert_eq!(view.scroll_by(100), 94);
    }

    #[test]
    fn scroll_to_clamps_and_clears_follow() {
        let mut view = view(10);
        view.state.update_layout(10, 4);
        view.scroll_to(2);
        assert_eq!(view.scroll_top(), 2);
        assert!(!view.state.is_following_end());
        view.scroll_to(1000);
        assert_eq!(view.scroll_top(), 6);
        assert!(view.state.is_following_end());
    }

    #[test]
    fn disable_follow_keeps_follow_end_off_at_the_bottom() {
        let mut view = view(10);
        view.state.update_layout(10, 4);
        view.state.scroll_to(
            6,
            ScrollToOptions {
                disable_follow: true,
            },
        );
        assert_eq!(view.scroll_top(), 6);
        assert!(!view.state.is_following_end());
    }

    #[test]
    fn always_scrollbar_stays_visible() {
        // `always` shows whenever the viewport has rows, even with short
        // content. `scroll-view.ts:83-90`.
        let mut view = view(10);
        view.state.update_layout(10, 4);
        assert!(view.state.is_scrollbar_visible());
        view.state.update_layout(2, 4);
        assert!(view.state.is_scrollbar_visible());
    }

    #[test]
    fn auto_scrollbar_needs_activity() {
        let mut view = ScrollView::new(
            Box::new(Text::new("a\nb\nc\nd", 0, 0)),
            ScrollViewOptions {
                scrollbar: Scrollbar::Auto,
                ..Default::default()
            },
        );
        view.state.update_layout(4, 2);
        assert!(!view.state.is_scrollbar_visible());
        view.state.set_scrollbar_active(true);
        assert!(view.state.is_scrollbar_visible());
        view.state.set_scrollbar(Scrollbar::Hidden);
        assert!(!view.state.is_scrollbar_visible());
    }
}
