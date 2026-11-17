//! The layout engine: a box tree with clips, painted into screen lines.
//!
//! Ground truth: `packages/tui/src/layout.ts` (449 lines) and
//! `layout-node.ts` (51 lines).
//!
//! Two pi features are out of scope here: Kitty image lines (no image
//! component in 4a) and OSC 133 zone markers (no shell integration yet).
//! Everything else, including scroll positioning, per-entry stack sizing and
//! scrollbar geometry, is ported.

use std::collections::HashMap;

use crate::ansi::{
    CURSOR_MARKER, extract_ansi_code, grapheme_cell_range, slice_by_column, visible_width,
};
use crate::component::{Component, composite_tui_line};
use crate::components::scroll_view::{RenderRequest, ScrollUpdate, ScrollViewState};
use crate::components::stack::{Align, LayoutViewport, StackEntryOptions};
use crate::mouse::ComponentId;

/// A rectangle in cells. `layout.ts:16-21`.
///
/// Coordinates are signed: a scroll view lays its child out at
/// `y - scrollTop` (`layout.ts:131`), which is negative above the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayoutRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

/// The tree the layout pass produces. `layout.ts:23-34`.
#[derive(Debug)]
pub struct LayoutBox {
    /// Where the box sits, relative to the frame's top-left corner.
    pub rect: LayoutRect,
    /// The visible region: [`intersect`] of the inherited clip and [`LayoutBox::rect`].
    pub clip: LayoutRect,
    /// Laid-out children, painted after this box's own lines.
    pub children: Vec<LayoutBox>,
    /// This box's own rendered lines, when it is a leaf.
    pub lines: Option<Vec<String>>,
    /// Rows dropped from the top of [`LayoutBox::lines`] so the cursor line stays visible.
    pub line_offset: usize,
    /// Scrollbar snapshot to paint, set only on a scroll box.
    pub scroll: Option<ScrollSnapshot>,
    /// Hit-test z-order; [`get_layout_boxes_at`] sorts on it, deepest first within a layer.
    pub layer: usize,
    /// `LayoutBox.component` (`layout.ts:24`): the component this box laid
    /// out. `dispatchMouseToLayout` dispatches the hit to it and then walks
    /// up `parent` (`tui-alt-screen.ts:836-853`).
    pub component: Option<ComponentId>,
    /// `LayoutBox.scrollView` (`layout.ts:29`): set only on a scroll box, and
    /// the identity `getScrollViewBox` matches against.
    pub scroll_view: Option<ComponentId>,
    /// `LayoutBox.scrollContentLines` (`layout.ts:30`): the unshifted
    /// content, which the Kitty cropper and the search corpus both read.
    pub scroll_content_lines: Option<Vec<String>>,
}

/// Scrollbar state captured for painting. `layout.ts:44-51`.
#[derive(Clone)]
pub struct ScrollSnapshot {
    pub scroll_top: usize,
    /// The scroll view's own answer to "is the scrollbar visible right now".
    pub is_visible: bool,
    pub is_active: bool,
    /// `true` for `Scrollbar::Always`: keep the cell background.
    pub preserve_background: bool,
    pub styles: crate::components::scroll_view::ScrollbarStyles,
}

impl std::fmt::Debug for ScrollSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScrollSnapshot")
            .field("scroll_top", &self.scroll_top)
            .field("is_visible", &self.is_visible)
            .field("is_active", &self.is_active)
            .finish_non_exhaustive()
    }
}

/// Scrollbar geometry. `layout.ts:44-51`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub column: usize,
    pub track_top: usize,
    pub track_height: usize,
    pub thumb_top: usize,
    pub thumb_height: usize,
    pub max_scroll_top: usize,
}

impl std::fmt::Debug for LayoutFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("lines", &self.lines)
            .finish_non_exhaustive()
    }
}

/// The result of laying out and painting. `layout.ts:36-42`.
pub struct LayoutFrame {
    pub root: LayoutBox,
    pub width: usize,
    pub height: usize,
    pub lines: Vec<String>,
    /// Pointer identity of the primary scroll view, if one exists.
    pub primary_scroll_view: Option<usize>,
    /// Scroll updates the measuring pass could not apply. The caller replays
    /// them with [`apply_scroll_updates`], because measuring only borrows.
    pub scroll_updates: Vec<(usize, ScrollUpdate)>,
}

/// What a component tells the layout engine about itself.
/// `layout-node.ts:42`.
pub enum LayoutNode<'a> {
    /// A plain component: its rendered lines are the layout.
    Leaf,
    /// Children stacked top to bottom, `gap` rows between entries.
    VStack {
        entries: Vec<(&'a dyn Component, StackEntryOptions)>,
        gap: usize,
        align: Align,
    },
    /// Children laid left to right, `gap` columns between entries.
    HStack {
        entries: Vec<(&'a dyn Component, StackEntryOptions)>,
        gap: usize,
        align: Align,
    },
    /// A scroll view wrapping `component`, driven by `state`.
    Scroll {
        component: &'a dyn Component,
        state: &'a ScrollViewState,
    },
}

/// `layout.ts:66-72`.
pub fn intersect(a: LayoutRect, b: LayoutRect) -> LayoutRect {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    LayoutRect {
        x,
        y,
        width: (right - x).max(0),
        height: (bottom - y).max(0),
    }
}

/// Component identity for the render cache. `layout.ts:76-84`.
fn component_id(component: &dyn Component) -> usize {
    std::ptr::from_ref::<dyn Component>(component).cast::<()>() as usize
}

struct LayoutContext {
    viewport: LayoutViewport,
    render_cache: HashMap<(usize, usize), Vec<String>>,
    request_render: Option<RenderRequest>,
    primary_scroll_view: Option<usize>,
    /// Scroll states to update once measuring finishes.
    pending: Vec<(usize, ScrollViewState, ScrollUpdate)>,
}

impl LayoutContext {
    /// `layout.ts:86-95`.
    fn render_cached(&mut self, component: &dyn Component, width: usize) -> Vec<String> {
        let safe_width = width.max(1);
        let key = (component_id(component), safe_width);
        if let Some(lines) = self.render_cache.get(&key) {
            return lines.clone();
        }
        let lines = component.render(safe_width);
        self.render_cache.insert(key, lines.clone());
        lines
    }

    /// `layout.ts:97-103`.
    fn measure_height(&mut self, component: &dyn Component, width: usize) -> usize {
        self.render_cached(component, width).len()
    }

    /// `layout.ts:105-107`.
    fn measure_width(&mut self, component: &dyn Component, width: usize) -> usize {
        self.render_cached(component, width)
            .iter()
            .map(|line| visible_width(line))
            .max()
            .unwrap_or(0)
    }
}

/// `layout.ts:296-327`.
fn replace_scrollbar_cell(
    line: &str,
    column: usize,
    total_width: usize,
    replacement: &str,
    preserve_target_background: bool,
) -> String {
    let (start, end) = grapheme_cell_range(line, column).unwrap_or((column, column + 1));
    let before = slice_by_column(line, 0, start, true);
    let target = slice_by_column(line, start, end - start, true);
    let after = slice_by_column(line, end, total_width.saturating_sub(end), true);

    // Collect the escape codes the target cell carries, so the replacement
    // inherits the cell's foreground. `layout.ts:311-319`.
    let mut target_prefix = String::new();
    let mut target_index = 0;
    while target_index < target.len() {
        match extract_ansi_code(&target, target_index) {
            Some(ansi) => {
                target_prefix.push_str(ansi);
                target_index += ansi.len();
            }
            None => break,
        }
    }
    let before_padding = " ".repeat(start.saturating_sub(visible_width(&before)));
    let cell_padding_before = " ".repeat(column.saturating_sub(start));
    let cell_padding_after = " ".repeat(end.saturating_sub(column + 1));
    let target_style = format!(
        "\x1b[0m\x1b]8;;\x07{}",
        if preserve_target_background {
            crate::ansi::active_background_ansi(&target_prefix)
        } else {
            String::new()
        }
    );
    format!(
        "{before}{before_padding}{target_style}{cell_padding_before}{replacement}{cell_padding_after}{after}"
    )
}

/// `layout.ts:280-303`.
pub fn get_scrollbar_geometry(
    box_: &LayoutBox,
    include_hidden_auto: bool,
) -> Option<ScrollbarGeometry> {
    let scroll = box_.scroll.as_ref()?;
    if box_.rect.width == 0 || box_.rect.height == 0 {
        return None;
    }
    let content_height = box_
        .children
        .first()
        .map_or_else(
            || box_.lines.as_ref().map_or(0, |lines| lines.len() as i64),
            |child| child.rect.height,
        )
        .max(0) as usize;
    let track_height = box_.rect.height.max(0) as usize;
    // `layout.ts:285-287`: an always-visible scrollbar needs no content, but
    // the hidden/auto modes only paint when the view says it is visible.
    let can_reveal_hidden_auto = include_hidden_auto && content_height > track_height;
    if !scroll.is_visible && !can_reveal_hidden_auto {
        return None;
    }
    let min_thumb_height = 2.min(track_height);
    let thumb_height = min_thumb_height
        .max((track_height * track_height) / content_height)
        .min(track_height);
    let max_scroll_top = content_height.saturating_sub(track_height);
    let max_thumb_top = track_height.saturating_sub(thumb_height);
    let thumb_offset = (scroll.scroll_top * max_thumb_top + max_scroll_top / 2)
        .checked_div(max_scroll_top)
        .unwrap_or(0);
    let column = box_.rect.x + box_.rect.width - 1;
    if column < box_.clip.x || column >= box_.clip.x + box_.clip.width || column < 0 {
        return None;
    }
    Some(ScrollbarGeometry {
        column: column as usize,
        track_top: box_.rect.y.max(0) as usize,
        track_height,
        thumb_top: box_.rect.y.max(0) as usize + thumb_offset,
        thumb_height,
        max_scroll_top,
    })
}

/// `layout.ts:305-322`.
fn paint_scrollbar(box_: &LayoutBox, screen: &mut [String], total_width: usize) {
    let (Some(scroll), Some(geometry)) =
        (box_.scroll.as_ref(), get_scrollbar_geometry(box_, false))
    else {
        return;
    };
    for offset in 0..geometry.track_height {
        let row = geometry.track_top + offset;
        if (row as i64) < box_.clip.y
            || (row as i64) >= box_.clip.y + box_.clip.height
            || row >= screen.len()
        {
            continue;
        }
        let is_thumb =
            row >= geometry.thumb_top && row < geometry.thumb_top + geometry.thumb_height;
        let replacement = if is_thumb {
            let glyph = if scroll.is_active { "█" } else { "┃" };
            (scroll.styles.thumb)(glyph)
        } else {
            (scroll.styles.track)("│")
        };
        screen[row] = replace_scrollbar_cell(
            &screen[row],
            geometry.column,
            total_width,
            &replacement,
            scroll.preserve_background,
        );
    }
}

/// `layout.ts:324-378`.
fn paint_box(box_: &LayoutBox, screen: &mut [String], total_width: usize) {
    if let Some(lines) = &box_.lines {
        let offset = box_.line_offset;
        let first_row = box_.rect.y.max(box_.clip.y).max(0);
        let last_row = (box_.rect.y + box_.rect.height)
            .min(box_.clip.y + box_.clip.height)
            .min(screen.len() as i64);
        for row in first_row..last_row {
            let Some(source_line) = lines.get((offset as i64 + row - box_.rect.y).max(0) as usize)
            else {
                continue;
            };
            // A full-width box may take the row as-is.
            if box_.rect.x == 0
                && box_.rect.width >= total_width as i64
                && screen[row as usize].is_empty()
            {
                screen[row as usize].clone_from(source_line);
            } else {
                screen[row as usize] = composite_tui_line(
                    &screen[row as usize],
                    source_line,
                    box_.rect.x.max(0) as usize,
                    box_.rect.width.max(0) as usize,
                    total_width,
                );
            }
        }
    }
    for child in &box_.children {
        paint_box(child, screen, total_width);
    }
    paint_scrollbar(box_, screen, total_width);
}

/// `layout.ts:110-243`.
// Render/layout walk mirrors the upstream TS function one-to-one; splitting
// it would obscure the port.
#[allow(clippy::too_many_lines)]
fn layout_component(
    context: &mut LayoutContext,
    component: &dyn Component,
    x: i64,
    y: i64,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
) -> LayoutBox {
    let safe_width = width.max(1);
    let Some(node) = component.layout_node() else {
        let lines = context.render_cached(component, safe_width);
        let allocated_height = height.unwrap_or(lines.len());
        let mut line_offset = 0;
        if lines.len() > allocated_height && allocated_height > 0 {
            // Keep the cursor line visible. `layout.ts:118-123`.
            let cursor_line = lines.iter().position(|line| line.contains(CURSOR_MARKER));
            if let Some(cursor_line) = cursor_line
                && cursor_line >= allocated_height
            {
                line_offset = cursor_line + 1 - allocated_height;
            }
        }
        return LayoutBox {
            rect: LayoutRect {
                x,
                y,
                width: safe_width as i64,
                height: allocated_height as i64,
            },
            clip: intersect(
                clip,
                LayoutRect {
                    x,
                    y,
                    width: safe_width as i64,
                    height: allocated_height as i64,
                },
            ),
            children: Vec::new(),
            lines: Some(lines),
            line_offset,
            scroll: None,
            layer: 0,
            component: Some(crate::component::component_id(component)),
            scroll_view: None,
            scroll_content_lines: None,
        };
    };

    match node {
        LayoutNode::Scroll {
            component: child,
            state,
        } => {
            let previous_scroll_top = state.scroll_top();
            let content_width = state.content_width(safe_width);
            let child_box = layout_component(
                context,
                child,
                x,
                y - previous_scroll_top as i64,
                content_width,
                None,
                clip,
            );
            let scroll_content_lines = child_box.lines.clone();
            let scroll_view_id = crate::component::component_id(component);
            let content_height = child_box.rect.height.max(0) as usize;
            let viewport_height = height.unwrap_or(content_height);
            let update = state.compute_scroll_update(content_height, viewport_height);
            let new_scroll_top = update.scroll_top;
            let update_changed = update.changed;
            let update_visible = update.state.is_scrollbar_visible();
            context
                .pending
                .push((component_id(component), state.clone(), update));
            if update_changed && let Some(request_render) = &context.request_render {
                // `layout.ts:153`: `updateLayout` also stores the callback and
                // asks for a frame when follow-end moved the viewport.
                request_render();
            }

            let mut child_box = child_box;
            // `layout.ts:156-157`: compensate for the scroll offset change.
            translate_box(
                &mut child_box,
                previous_scroll_top as i64 - new_scroll_top as i64,
            );

            let rect = LayoutRect {
                x,
                y,
                width: safe_width as i64,
                height: viewport_height as i64,
            };
            let child_clip = intersect(clip, rect);
            // The scroll view paints its child, not its own lines: pi keeps
            // the unshifted content in `scrollContentLines` for image
            // cropping only (`layout.ts:157`), which this port does not have.
            let mut box_ = LayoutBox {
                rect,
                clip: child_clip,
                children: vec![child_box],
                lines: None,
                line_offset: 0,
                scroll: Some(ScrollSnapshot {
                    scroll_top: new_scroll_top,
                    is_visible: update_visible,
                    is_active: state.is_scrollbar_active(),
                    preserve_background: state.scrollbar()
                        == crate::components::scroll_view::Scrollbar::Always,
                    styles: state.styles.clone(),
                }),
                layer: 0,
                component: Some(scroll_view_id),
                scroll_view: Some(scroll_view_id),
                scroll_content_lines,
            };
            update_clips_children(&mut box_, child_clip);
            if state.primary() || context.primary_scroll_view.is_none() {
                context.primary_scroll_view = Some(scroll_view_id);
            }
            box_
        }
        LayoutNode::VStack {
            entries,
            gap,
            align,
        } => {
            let _ = align;
            let entries: Vec<(&dyn Component, StackEntryOptions)> = entries
                .into_iter()
                .filter(|(_, options)| options.is_visible(context.viewport))
                .collect();
            let gap_total = entries.len().saturating_sub(1) * gap;
            let intrinsic_heights: Vec<usize> = entries
                .iter()
                .map(|(child, options)| match options.basis {
                    Some(basis) => basis,
                    None => context.measure_height(*child, safe_width),
                })
                .collect();
            let sizes = crate::components::stack::allocate_stack_sizes(
                &entries,
                &intrinsic_heights,
                height,
                gap,
            );
            let natural_height = sizes.iter().sum::<usize>() + gap_total;
            let allocated_height = height.unwrap_or(natural_height);
            let rect = LayoutRect {
                x,
                y,
                width: safe_width as i64,
                height: allocated_height as i64,
            };
            let clip_rect = intersect(clip, rect);
            let mut box_ = LayoutBox {
                rect,
                clip: clip_rect,
                children: Vec::new(),
                lines: None,
                line_offset: 0,
                scroll: None,
                layer: 0,
                component: Some(crate::component::component_id(component)),
                scroll_view: None,
                scroll_content_lines: None,
            };
            let mut child_y = y;
            for (index, (child, _)) in entries.iter().enumerate() {
                box_.children.push(layout_component(
                    context,
                    *child,
                    x,
                    child_y,
                    safe_width,
                    Some(sizes[index]),
                    clip_rect,
                ));
                child_y += sizes[index] as i64 + gap as i64;
            }
            box_
        }
        LayoutNode::HStack {
            entries,
            gap,
            align,
        } => {
            let entries: Vec<(&dyn Component, StackEntryOptions)> = entries
                .into_iter()
                .filter(|(_, options)| options.is_visible(context.viewport))
                .collect();
            let intrinsic_widths: Vec<usize> = entries
                .iter()
                .map(|(child, options)| match options.basis {
                    Some(basis) => basis,
                    None => context.measure_width(*child, safe_width),
                })
                .collect();
            let widths = crate::components::stack::allocate_stack_sizes(
                &entries,
                &intrinsic_widths,
                Some(safe_width),
                gap,
            );
            let intrinsic_heights: Vec<usize> = entries
                .iter()
                .enumerate()
                .map(|(index, (child, _))| context.measure_height(*child, widths[index].max(1)))
                .collect();
            let allocated_height = match height {
                Some(height) => height,
                None => intrinsic_heights.iter().copied().max().unwrap_or(0),
            };
            let rect = LayoutRect {
                x,
                y,
                width: safe_width as i64,
                height: allocated_height as i64,
            };
            let clip_rect = intersect(clip, rect);
            let mut box_ = LayoutBox {
                rect,
                clip: clip_rect,
                children: Vec::new(),
                lines: None,
                line_offset: 0,
                scroll: None,
                layer: 0,
                component: Some(crate::component::component_id(component)),
                scroll_view: None,
                scroll_content_lines: None,
            };
            let mut child_x = x;
            for (index, (child, _)) in entries.iter().enumerate() {
                let natural_child_height = intrinsic_heights[index];
                let child_height = match align {
                    Align::Stretch => allocated_height,
                    Align::Start | Align::Center | Align::End => {
                        natural_child_height.min(allocated_height)
                    }
                };
                let mut child_y = y;
                match align {
                    Align::Center => {
                        child_y += allocated_height.saturating_sub(child_height) as i64 / 2;
                    }
                    Align::End => child_y += allocated_height.saturating_sub(child_height) as i64,
                    _ => {}
                }
                let child_width = widths[index];
                if child_width == 0 {
                    box_.children.push(LayoutBox {
                        rect: LayoutRect {
                            x: child_x,
                            y: child_y,
                            width: 0,
                            height: child_height as i64,
                        },
                        clip: LayoutRect {
                            x: child_x,
                            y: child_y,
                            width: 0,
                            height: 0,
                        },
                        children: Vec::new(),
                        lines: None,
                        line_offset: 0,
                        scroll: None,
                        layer: 0,
                        component: None,
                        scroll_view: None,
                        scroll_content_lines: None,
                    });
                } else {
                    box_.children.push(layout_component(
                        context,
                        *child,
                        child_x,
                        child_y,
                        child_width,
                        Some(child_height),
                        clip_rect,
                    ));
                }
                child_x += child_width as i64 + gap as i64;
            }
            box_
        }
        LayoutNode::Leaf => unreachable!("leaf components are handled before the node lookup"),
    }
}

/// `layout.ts:115-117`.
fn translate_box(box_: &mut LayoutBox, delta_y: i64) {
    box_.rect.y += delta_y;
    for child in &mut box_.children {
        translate_box(child, delta_y);
    }
}

/// `layout.ts:119-123`.
fn update_clips_children(box_: &mut LayoutBox, parent_clip: LayoutRect) {
    box_.clip = intersect(parent_clip, box_.rect);
    for child in &mut box_.children {
        update_clips_children(child, box_.clip);
    }
}

/// `layout.ts:388-411`.
// Public entry point; taking `Option<RenderRequest>` by value keeps the
// signature aligned with the upstream TS parameter.
#[allow(clippy::needless_pass_by_value)]
pub fn render_layout_frame(
    root: &dyn Component,
    width: usize,
    height: usize,
    request_render: Option<RenderRequest>,
) -> LayoutFrame {
    let safe_width = width.max(1);
    let safe_height = height.max(1);
    let mut context = LayoutContext {
        viewport: LayoutViewport {
            width: safe_width,
            height: safe_height,
        },
        render_cache: HashMap::new(),
        request_render: request_render.clone(),
        primary_scroll_view: None,
        pending: Vec::new(),
    };
    let viewport_rect = LayoutRect {
        x: 0,
        y: 0,
        width: safe_width as i64,
        height: safe_height as i64,
    };
    let root_box = layout_component(
        &mut context,
        root,
        0,
        0,
        safe_width,
        Some(safe_height),
        viewport_rect,
    );
    let mut lines: Vec<String> = vec![String::new(); safe_height];
    paint_box(&root_box, &mut lines, safe_width);
    let scroll_updates: Vec<(usize, ScrollUpdate)> = context
        .pending
        .into_iter()
        .map(|(id, _state, mut update)| {
            update.state.set_render_request(request_render.clone());
            (id, update)
        })
        .collect();
    LayoutFrame {
        root: root_box,
        width: safe_width,
        height: safe_height,
        lines,
        primary_scroll_view: context.primary_scroll_view,
        scroll_updates,
    }
}

/// Replay the scroll updates a frame measured, walking the same tree.
///
/// pi applies these inline (`layout.ts:153`); the split is a Rust borrowing
/// artifact and lands the same scroll positions.
pub fn apply_scroll_updates(root: &mut dyn Component, updates: &[(usize, ScrollUpdate)]) -> usize {
    let mut applied = 0;
    let id = component_id(root);
    if let Some((_, update)) = updates.iter().find(|(update_id, _)| *update_id == id)
        && let Some(state) = root.scroll_state_mut()
    {
        state.apply_scroll_update(update.clone());
        applied += 1;
    }
    for child in root.layout_children_mut() {
        applied += apply_scroll_updates(child, updates);
    }
    applied
}

/// `layout.ts:415-425`.
fn contains_point(rect: LayoutRect, x: i64, y: i64) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

/// Visual hit path, deepest component first. `layout.ts:415-425`.
pub fn get_layout_boxes_at(frame: &LayoutFrame, x: i64, y: i64) -> Vec<&LayoutBox> {
    fn visit<'a>(
        box_: &'a LayoutBox,
        x: i64,
        y: i64,
        depth: usize,
        out: &mut Vec<(&'a LayoutBox, usize)>,
    ) {
        if !contains_point(box_.clip, x, y) {
            return;
        }
        out.push((box_, depth));
        for child in &box_.children {
            visit(child, x, y, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    visit(&frame.root, x, y, 0, &mut out);
    out.sort_by(|a, b| b.0.layer.cmp(&a.0.layer).then(b.1.cmp(&a.1)));
    out.into_iter().map(|(box_, _)| box_).collect()
}

/// Scroll views whose rect contains the point, deepest first.
/// `layout.ts:439-449`.
///
/// pi keys the result on the `ScrollView` object; here that is the scroll box's
/// own [`LayoutBox::scroll_view`] identity, which is also what
/// [`get_scroll_view_box`] matches. `LayoutBox::scroll` carries only the
/// painted snapshot, so it is the wrong key.
pub fn get_scroll_views_at(frame: &LayoutFrame, x: i64, y: i64) -> Vec<ComponentId> {
    fn visit(box_: &LayoutBox, x: i64, y: i64, depth: usize, out: &mut Vec<(ComponentId, usize)>) {
        if !contains_point(box_.clip, x, y) {
            return;
        }
        if let Some(scroll_view) = box_.scroll_view
            && contains_point(box_.rect, x, y)
        {
            out.push((scroll_view, depth));
        }
        for child in &box_.children {
            visit(child, x, y, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    visit(&frame.root, x, y, 0, &mut out);
    out.sort_by_key(|(_, depth)| std::cmp::Reverse(*depth));
    out.into_iter().map(|(id, _)| id).collect()
}

/// The box a scroll view laid out into. `getScrollViewBox`, `layout.ts:427-437`.
pub fn get_scroll_view_box(frame: &LayoutFrame, scroll_view: ComponentId) -> Option<&LayoutBox> {
    fn visit(box_: &LayoutBox, scroll_view: ComponentId) -> Option<&LayoutBox> {
        if box_.scroll_view == Some(scroll_view) {
            return Some(box_);
        }
        box_.children
            .iter()
            .find_map(|child| visit(child, scroll_view))
    }
    visit(&frame.root, scroll_view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::scroll_view::{ScrollView, ScrollViewOptions, Scrollbar};
    use crate::components::stack::{StackEntryOptions, VStack};
    use crate::components::text::Text;
    use std::sync::{Arc, Mutex};

    struct Fixed(usize);

    impl Component for Fixed {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![String::new(); self.0]
        }
    }

    struct Rows(usize);

    impl Component for Rows {
        fn render(&self, _width: usize) -> Vec<String> {
            (0..self.0).map(|row| format!("line {row:02}")).collect()
        }
    }

    /// `translateBox(childBox, previousScrollTop - state.scrollTop)`,
    /// `layout.ts:156`: when `followEnd` moves the viewport down by N on the
    /// first frame, the child box moves up by N, so the newest lines land
    /// inside the clip. With the sign reversed the box is pushed out of the
    /// viewport and the frame paints nothing at all.
    #[test]
    fn follow_end_translates_the_child_box_up_by_the_scroll_delta() {
        let view = ScrollView::new(
            Box::new(Rows(10)),
            ScrollViewOptions {
                follow_end: true,
                primary: true,
                scrollbar: Scrollbar::Hidden,
                ..Default::default()
            },
        );
        let frame = render_layout_frame(&view, 10, 4, None);
        assert_eq!(
            frame.lines,
            vec!["line 06", "line 07", "line 08", "line 09"],
            "the newest screenful is painted"
        );
    }

    #[test]
    fn intersect_keeps_the_overlap() {
        let a = LayoutRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let b = LayoutRect {
            x: 5,
            y: 5,
            width: 10,
            height: 10,
        };
        assert_eq!(
            intersect(a, b),
            LayoutRect {
                x: 5,
                y: 5,
                width: 5,
                height: 5
            }
        );
        let c = LayoutRect {
            x: 20,
            y: 20,
            width: 1,
            height: 1,
        };
        assert_eq!(
            intersect(a, c),
            LayoutRect {
                x: 20,
                y: 20,
                width: 0,
                height: 0
            }
        );
    }

    #[test]
    fn leaf_layout_takes_the_viewport_height() {
        // The root is laid out with an explicit height (`layout.ts:400`), so
        // its rect is the viewport even when it renders fewer lines.
        let root = Fixed(3);
        let frame = render_layout_frame(&root, 10, 6, None);
        assert_eq!(frame.root.rect.height, 6);
        assert_eq!(frame.root.lines.as_ref().unwrap().len(), 3);
        assert_eq!(frame.root.clip.width, 10);
        assert_eq!(frame.lines.len(), 6);
    }

    #[test]
    fn vstack_allocates_growth_to_grow_entries() {
        let mut stack = VStack::new(vec![]);
        stack.add_child(Box::new(Fixed(1)), StackEntryOptions::default());
        stack.add_child(
            Box::new(Fixed(1)),
            StackEntryOptions {
                grow: Some(1),
                ..Default::default()
            },
        );
        let frame = render_layout_frame(&stack, 10, 6, None);
        assert_eq!(frame.root.children[0].rect.height, 1);
        assert_eq!(frame.root.children[1].rect.height, 5);
        assert_eq!(frame.root.children[1].rect.y, 1);
    }

    #[test]
    fn hstack_lays_children_side_by_side_with_a_gap() {
        struct Cells(&'static str, usize);
        impl Component for Cells {
            fn render(&self, _width: usize) -> Vec<String> {
                vec![self.0.to_string(); self.1]
            }
        }
        let mut stack = crate::components::stack::HStack::new(vec![]);
        stack.stack().set_gap(2);
        stack.add_child(Box::new(Cells("ab", 1)), StackEntryOptions::default());
        stack.add_child(Box::new(Cells("cd", 1)), StackEntryOptions::default());
        let frame = render_layout_frame(&stack, 20, 2, None);
        assert_eq!(frame.root.children[0].rect.x, 0);
        assert_eq!(frame.root.children[0].rect.width, 2);
        assert_eq!(frame.root.children[1].rect.x, 4);
        assert_eq!(frame.root.children[1].rect.width, 2);
    }

    #[test]
    fn hidden_entries_are_skipped() {
        let mut stack = VStack::new(vec![]);
        stack.add_child(
            Box::new(Fixed(2)),
            StackEntryOptions {
                visible: Some(|_| false),
                ..Default::default()
            },
        );
        stack.add_child(Box::new(Fixed(1)), StackEntryOptions::default());
        let frame = render_layout_frame(&stack, 10, 6, None);
        assert_eq!(frame.root.children.len(), 1);
    }

    #[test]
    fn cursor_marker_scrolls_into_view() {
        // A one-line viewport must show the last line carrying the marker.
        struct WithCursor(usize);
        impl Component for WithCursor {
            fn render(&self, width: usize) -> Vec<String> {
                let mut lines = vec![String::new(); self.0];
                let line = self.0 - 1;
                lines[line] = format!("{}{}", crate::ansi::CURSOR_MARKER, "x");
                let _ = width;
                lines
            }
        }
        let text = Text::new("a\nb\nc", 0, 0);
        let root = WithCursor(3);
        let frame = render_layout_frame(&root, 10, 1, None);
        assert_eq!(frame.root.line_offset, 2);
        let _ = text;
    }

    #[test]
    fn scroll_view_offsets_content_and_paints_a_scrollbar() {
        let mut view = ScrollView::new(
            Box::new(Text::new("0\n1\n2\n3\n4\n5", 0, 0)),
            ScrollViewOptions {
                scrollbar: crate::components::scroll_view::Scrollbar::Always,
                ..Default::default()
            },
        );
        view.update_layout(6, 3);
        view.scroll_to(2);
        let frame = render_layout_frame(&view, 6, 3, None);
        assert!(frame.root.scroll.is_some());
        // The child is laid out at `y - scrollTop` (`layout.ts:131`) and the
        // clip keeps only the visible rows.
        assert_eq!(frame.root.children[0].rect.y, -2);
        assert!(strip(&frame.lines[0]).starts_with('2'));
        assert!(strip(&frame.lines[1]).starts_with('3'));
        assert!(strip(&frame.lines[2]).starts_with('4'));
        // The scrollbar owns the last column.
        assert!(get_scrollbar_geometry(&frame.root, false).is_some());
    }

    fn strip(line: &str) -> String {
        crate::ansi::strip_terminal_sequences(line)
    }

    #[test]
    fn scroll_view_follow_end_applies_on_the_next_pass() {
        let mut view = ScrollView::new(
            Box::new(Text::new("0\n1\n2\n3\n4\n5", 0, 0)),
            ScrollViewOptions {
                follow_end: true,
                ..Default::default()
            },
        );
        // Measuring only borrows, so follow-end moves once the host applies
        // the updates the frame reports.
        let frame = render_layout_frame(&view, 6, 3, None);
        assert_eq!(view.scroll_top(), 0);
        assert_eq!(frame.root.children[0].rect.height, 6);
        assert_eq!(frame.scroll_updates.len(), 1);

        let applied = apply_scroll_updates(&mut view, &frame.scroll_updates);
        assert_eq!(applied, 1);
        assert_eq!(view.scroll_top(), 3);

        let frame = render_layout_frame(&view, 6, 3, None);
        assert!(strip(&frame.lines[0]).starts_with('3'));
    }

    #[test]
    fn scrollbar_geometry_scales_the_thumb() {
        let mut view = ScrollView::new(
            Box::new(Text::new("0\n1\n2\n3\n4\n5\n6\n7\n8\n9", 0, 0)),
            ScrollViewOptions {
                scrollbar: crate::components::scroll_view::Scrollbar::Always,
                ..Default::default()
            },
        );
        view.update_layout(10, 5);
        view.scroll_to(5);
        let frame = render_layout_frame(&view, 6, 5, None);
        let geometry = get_scrollbar_geometry(&frame.root, false).expect("geometry");
        assert_eq!(geometry.track_height, 5);
        assert_eq!(geometry.max_scroll_top, 5);
        assert!(geometry.thumb_height >= 2);
        assert_eq!(geometry.column, 5);
    }

    #[test]
    fn hit_testing_returns_the_deepest_box() {
        let mut stack = VStack::new(vec![]);
        stack.add_child(Box::new(Fixed(2)), StackEntryOptions::default());
        stack.add_child(Box::new(Fixed(2)), StackEntryOptions::default());
        let frame = render_layout_frame(&stack, 10, 4, None);
        let path = get_layout_boxes_at(&frame, 3, 1);
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].rect.y, 0);
        assert!(get_layout_boxes_at(&frame, 3, 9).is_empty());
    }

    #[test]
    fn paint_respects_clips() {
        struct Cells(&'static str);
        impl Component for Cells {
            fn render(&self, _width: usize) -> Vec<String> {
                vec![self.0.to_string()]
            }
        }
        let mut stack = VStack::new(vec![]);
        let fixed = || StackEntryOptions {
            basis: Some(1),
            min_size: Some(1),
            ..Default::default()
        };
        stack.add_child(Box::new(Cells("a")), fixed());
        stack.add_child(Box::new(Cells("b")), fixed());
        stack.add_child(Box::new(Cells("c")), fixed());
        // A two-row viewport over three one-row children clips the third;
        // `min_size` keeps the shrinker from stealing rows from the first two.
        let frame = render_layout_frame(&stack, 4, 2, None);
        assert_eq!(frame.lines.len(), 2);
        assert_eq!(frame.lines[0], "a");
        assert_eq!(frame.lines[1], "b");
    }

    #[test]
    fn render_requests_are_forwarded() {
        let calls = Arc::new(Mutex::new(0usize));
        let sink = calls.clone();
        let view = ScrollView::new(
            Box::new(Text::new("0\n1\n2\n3", 0, 0)),
            ScrollViewOptions {
                follow_end: true,
                ..Default::default()
            },
        );
        let request: RenderRequest = Arc::new(move || {
            *sink.lock().unwrap() += 1;
        });
        render_layout_frame(&view, 6, 2, Some(request));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    /// `LayoutBox.scrollView` / `scrollContentLines` / `component`
    /// (`layout.ts:24,29-30`). pi keys `getScrollViewBox` and
    /// `getScrollViewsAt` on the `ScrollView` object; the painted snapshot in
    /// `scroll` cannot stand in for it, so the scroll box carries its own
    /// identity plus the unshifted content lines.
    #[test]
    fn scroll_boxes_carry_their_identity_and_unshifted_content() {
        let view = ScrollView::new(
            Box::new(Text::new("0\n1\n2\n3", 0, 0)),
            ScrollViewOptions {
                follow_end: true,
                ..Default::default()
            },
        );
        let id = component_id(&view);
        let frame = render_layout_frame(&view, 6, 2, None);
        assert_eq!(
            frame.primary_scroll_view,
            Some(id),
            "the only scroll view is primary"
        );

        // The box is found by identity, not by its scroll position.
        let box_ = get_scroll_view_box(&frame, id).expect("the scroll view's box");
        assert_eq!(
            box_.rect,
            LayoutRect {
                x: 0,
                y: 0,
                width: 6,
                height: 2
            }
        );
        assert_eq!(
            box_.scroll.as_ref().map(|s| s.scroll_top),
            Some(2),
            "follow-end parked at the bottom"
        );

        // `scrollContentLines` is the whole content, unshifted by scrollTop:
        // the Kitty cropper and the search corpus both index into it.
        // The child's own rendered lines: padded to the content width, exactly
        // what the viewport would have painted without the scroll shift.
        assert_eq!(
            box_.scroll_content_lines.as_deref(),
            Some(
                [
                    "0     ".to_string(),
                    "1     ".to_string(),
                    "2     ".to_string(),
                    "3     ".to_string()
                ]
                .as_slice()
            )
        );

        // `getScrollViewsAt` returns that identity for a point inside the
        // viewport, and nothing for a point on the track outside it.
        assert_eq!(get_scroll_views_at(&frame, 0, 0), vec![id]);
        assert_eq!(get_scroll_views_at(&frame, 5, 1), vec![id]);
        assert!(get_scroll_views_at(&frame, 0, 9).is_empty());
        assert!(get_scroll_view_box(&frame, id + 1).is_none());
    }

    /// pi's `getScrollViewsAt` sorts by depth, so a nested scroll view is
    /// offered the wheel delta first (`tui-alt-screen.ts:989-994`).
    #[test]
    fn nested_scroll_views_are_offered_deepest_first() {
        // Boxed so the identity is the heap address and survives the move into
        // the outer view: a stack slot would be recycled by the time the frame
        // records it.
        let inner = Box::new(ScrollView::new(
            Box::new(Text::new("a\nb\nc\nd", 0, 0)),
            ScrollViewOptions {
                follow_end: false,
                ..Default::default()
            },
        ));
        let inner_id = component_id(&*inner);
        let outer = ScrollView::new(
            inner,
            ScrollViewOptions {
                follow_end: false,
                ..Default::default()
            },
        );
        let outer_id = component_id(&outer);
        let frame = render_layout_frame(&outer, 6, 2, None);
        let at = get_scroll_views_at(&frame, 0, 1);
        assert_eq!(at, vec![inner_id, outer_id], "deepest first");
        assert_eq!(
            get_scroll_view_box(&frame, outer_id).map(|b| b.rect.height),
            Some(2)
        );
    }
}
