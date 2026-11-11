//! `Box`: padded, optionally background-filled container.
//! `packages/tui/src/components/box.ts`.

use crate::ansi::visible_width;
use crate::component::{Component, dispatch_mouse_band};
use crate::mouse::{MouseEvent, MouseEventResult};
use std::cell::RefCell;

/// The background painter `Box` applies to every rendered line.
pub type BackgroundFn = Box<dyn Fn(&str) -> String + Send + Sync>;

#[derive(Default)]
pub struct BoxComponent {
    children: Vec<Box<dyn Component>>,
    padding_x: usize,
    padding_y: usize,
    bg: Option<BackgroundFn>,
    /// `(content width, child heights)` from the last render.
    /// `box.ts:56-58`.
    mouse_layout: RefCell<Option<(usize, Vec<usize>)>>,
}

impl BoxComponent {
    /// `box.ts:44-47`.
    pub fn new(padding_x: usize, padding_y: usize) -> Self {
        Self {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg: None,
            mouse_layout: RefCell::default(),
        }
    }

    /// `box.ts:44-47`.
    pub fn with_bg(
        padding_x: usize,
        padding_y: usize,
        bg: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg: Some(Box::new(bg)),
            mouse_layout: RefCell::default(),
        }
    }

    /// `box.ts:49-52`.
    pub fn add_child(&mut self, component: Box<dyn Component>) {
        self.children.push(component);
    }

    /// `box.ts:54-60`.
    pub fn remove_child(&mut self, index: usize) -> Box<dyn Component> {
        self.children.remove(index)
    }

    /// `box.ts:62-64`.
    pub fn clear(&mut self) {
        self.children.clear();
    }

    pub fn len(&self) -> usize {
        self.children.len()
    }

    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&crate::component::DynComponent> {
        self.children.get(index).map(AsRef::as_ref)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut crate::component::DynComponent> {
        self.children.get_mut(index).map(AsMut::as_mut)
    }

    /// `box.ts:157-167`.
    fn apply_bg(&self, line: &str, width: usize) -> String {
        let pad = width.saturating_sub(visible_width(line));
        let padded = format!("{}{}", line, " ".repeat(pad));
        match &self.bg {
            Some(bg) => bg(&padded),
            None => padded,
        }
    }
}

impl Component for BoxComponent {
    /// `box.ts:93-155`.
    fn render(&self, width: usize) -> Vec<String> {
        if self.children.is_empty() {
            return Vec::new();
        }
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        let left_pad = " ".repeat(self.padding_x);

        let mut child_lines: Vec<String> = Vec::new();
        let mut heights = Vec::with_capacity(self.children.len());
        for child in &self.children {
            let lines = child.render(content_width);
            heights.push(lines.len());
            child_lines.extend(lines);
        }
        *self.mouse_layout.borrow_mut() = Some((content_width, heights));
        if child_lines.is_empty() {
            return Vec::new();
        }

        let mut result: Vec<String> = Vec::new();
        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }
        for line in &child_lines {
            result.push(self.apply_bg(&format!("{left_pad}{line}"), width));
        }
        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }
        crate::ansi::flatten_lines(&mut result);
        result
    }

    fn invalidate(&mut self) {
        *self.mouse_layout.borrow_mut() = None;
        for child in &mut self.children {
            child.invalidate();
        }
    }

    /// `Box.handleMouse`, `box.ts:75-102`. Content coordinates are shifted by
    /// the padding; a press inside the padding belongs to the box, not a child.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        let content_width = (event.width as i64 - (self.padding_x * 2) as i64).max(1) as usize;
        let content_y = event.y - self.padding_y as i64;
        let content_x = event.x - self.padding_x as i64;
        if content_y < 0 || content_x < 0 || content_x >= content_width as i64 {
            return None;
        }
        let cached = self.mouse_layout.borrow().clone();
        let host_id = crate::mouse::component_id(&*self);
        dispatch_mouse_band(
            host_id,
            false,
            &mut self.children,
            cached.as_ref(),
            event,
            content_x,
            content_y,
            content_width,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_terminal_sequences;
    use crate::components::text::Text;

    #[test]
    fn empty_box_renders_nothing() {
        assert!(BoxComponent::new(1, 1).render(10).is_empty());
    }

    #[test]
    fn pads_children_horizontally_and_vertically() {
        let mut boxed = BoxComponent::new(1, 1);
        boxed.add_child(Box::new(Text::new("hi", 0, 0)));
        let rendered: Vec<String> = boxed
            .render(6)
            .iter()
            .map(|l| strip_terminal_sequences(l))
            .collect();
        assert_eq!(rendered, vec!["      ", " hi   ", "      "]);
    }

    #[test]
    fn children_render_at_the_content_width() {
        let mut boxed = BoxComponent::new(2, 0);
        boxed.add_child(Box::new(Text::new("abcd", 0, 0)));
        // Content width is 6 - 2 * 2 = 2, so "abcd" wraps to two lines.
        let rendered = boxed.render(6);
        assert_eq!(strip_terminal_sequences(&rendered[0]), "  ab  ");
        assert_eq!(strip_terminal_sequences(&rendered[1]), "  cd  ");
    }

    #[test]
    fn background_covers_padding_too() {
        let mut boxed = BoxComponent::with_bg(1, 0, |line| format!("[{line}]"));
        boxed.add_child(Box::new(Text::new("x", 0, 0)));
        assert_eq!(boxed.render(4)[0], "[ x  ]");
    }

    #[test]
    fn multiple_children_stack() {
        let mut boxed = BoxComponent::new(0, 0);
        boxed.add_child(Box::new(Text::new("a", 0, 0)));
        boxed.add_child(Box::new(Text::new("b", 0, 0)));
        assert_eq!(boxed.render(3).len(), 2);
    }
}
