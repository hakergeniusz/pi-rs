//! `Stack` / `VStack` / `HStack`: entry sizing for the layout engine.
//! `packages/tui/src/components/stack.ts`, `v-stack.ts`, `h-stack.ts`.
//!
//! A stack renders as plain vertical concatenation (pi inherits that from
//! `Container`); the row/column behaviour lives in the layout engine, which
//! reads the entries through [`crate::layout::LayoutNode`].

use crate::component::Component;
use crate::layout::LayoutNode;

/// Size constraints for one stack entry. `stack.ts:10-17`.
///
/// No `PartialEq`: `visible` is a bare function pointer, and comparing
/// function pointers is meaningless (`visible` is only ever called).
#[derive(Debug, Clone, Copy)]
pub struct StackEntryOptions {
    /// Fixed size, or `None` for "auto" (measure the component).
    pub basis: Option<usize>,
    pub grow: Option<usize>,
    pub shrink: Option<usize>,
    pub min_size: Option<usize>,
    pub max_size: Option<usize>,
    pub visible: Option<fn(&LayoutViewport) -> bool>,
}

impl StackEntryOptions {
    /// pi's defaults: auto basis, no grow, shrink 1. `stack.ts:31-38`.
    pub fn new() -> Self {
        Self {
            basis: None,
            grow: Some(0),
            shrink: Some(1),
            min_size: Some(0),
            max_size: None,
            visible: None,
        }
    }

    /// `stack.ts:72-76`.
    pub fn is_visible(&self, viewport: LayoutViewport) -> bool {
        match self.visible {
            Some(visible) => visible(&viewport),
            None => true,
        }
    }
}

impl Default for StackEntryOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Cross-axis alignment. `stack.ts:22-25`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Stretch,
    Start,
    Center,
    End,
}

/// The viewport handed to `visible` callbacks. `layout-node.ts:5-8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutViewport {
    pub width: usize,
    pub height: usize,
}

/// Stack flavour, used by the layout node. `stack.ts:41`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackType {
    Vertical,
    Horizontal,
}

/// A stack of components with gap and alignment. `stack.ts:41-88`.
pub struct Stack {
    children: Vec<Box<dyn Component>>,
    entries: Vec<StackEntryOptions>,
    gap: usize,
    align: Align,
    layout_type: StackType,
}

impl Stack {
    /// `stack.ts:49-59`.
    pub fn new(gap: usize, align: Align, layout_type: StackType) -> Self {
        Self {
            children: Vec::new(),
            entries: Vec::new(),
            gap,
            align,
            layout_type,
        }
    }

    /// `stack.ts:68-85`.
    pub fn add_child(&mut self, component: Box<dyn Component>, options: StackEntryOptions) {
        self.children.push(component);
        self.entries.push(options);
    }

    /// `stack.ts:87-92`.
    pub fn remove_child(&mut self, index: usize) -> Box<dyn Component> {
        self.entries.remove(index);
        self.children.remove(index)
    }

    /// `stack.ts:94-97`.
    pub fn clear(&mut self) {
        self.children.clear();
        self.entries.clear();
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

    /// Change the gap between entries. `stack.ts:51`.
    pub fn set_gap(&mut self, gap: usize) {
        self.gap = gap;
    }

    pub fn gap(&self) -> usize {
        self.gap
    }

    pub fn align(&self) -> Align {
        self.align
    }

    pub fn layout_type(&self) -> StackType {
        self.layout_type
    }

    fn entries_ref(&self) -> Vec<(&dyn Component, StackEntryOptions)> {
        self.children
            .iter()
            .zip(self.entries.iter())
            .map(|(child, options)| (child.as_ref(), *options))
            .collect()
    }
}

impl Component for Stack {
    /// Container concatenation. `tui.ts:394-406`.
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

    /// `stack.ts:101-108`.
    fn layout_node(&self) -> Option<LayoutNode<'_>> {
        let entries = self.entries_ref();
        Some(match self.layout_type {
            StackType::Vertical => LayoutNode::VStack {
                entries,
                gap: self.gap,
                align: self.align,
            },
            StackType::Horizontal => LayoutNode::HStack {
                entries,
                gap: self.gap,
                align: self.align,
            },
        })
    }

    fn layout_children_mut(&mut self) -> Vec<&mut crate::component::DynComponent> {
        self.children.iter_mut().map(AsMut::as_mut).collect()
    }
}

/// Vertical stack. `v-stack.ts`.
pub struct VStack {
    inner: Stack,
}

impl VStack {
    pub fn new(children: Vec<Box<dyn Component>>) -> Self {
        Self::with_gap(children, 0)
    }

    pub fn with_gap(children: Vec<Box<dyn Component>>, gap: usize) -> Self {
        let mut stack = Stack::new(gap, Align::Stretch, StackType::Vertical);
        for child in children {
            stack.add_child(child, StackEntryOptions::default());
        }
        Self { inner: stack }
    }

    pub fn stack(&mut self) -> &mut Stack {
        &mut self.inner
    }

    pub fn add_child(&mut self, component: Box<dyn Component>, options: StackEntryOptions) {
        self.inner.add_child(component, options);
    }

    pub fn render_lines(&self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }
}

impl Component for VStack {
    fn render(&self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }

    fn layout_node(&self) -> Option<LayoutNode<'_>> {
        self.inner.layout_node()
    }

    fn layout_children_mut(&mut self) -> Vec<&mut crate::component::DynComponent> {
        self.inner.layout_children_mut()
    }
}

/// Horizontal stack. `h-stack.ts`.
pub struct HStack {
    inner: Stack,
}

impl HStack {
    pub fn new(children: Vec<Box<dyn Component>>) -> Self {
        Self::with_gap(children, 0)
    }

    pub fn with_gap(children: Vec<Box<dyn Component>>, gap: usize) -> Self {
        let mut stack = Stack::new(gap, Align::Stretch, StackType::Horizontal);
        for child in children {
            stack.add_child(child, StackEntryOptions::default());
        }
        Self { inner: stack }
    }

    pub fn stack(&mut self) -> &mut Stack {
        &mut self.inner
    }

    pub fn add_child(&mut self, component: Box<dyn Component>, options: StackEntryOptions) {
        self.inner.add_child(component, options);
    }
}

impl Component for HStack {
    fn render(&self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }

    fn layout_node(&self) -> Option<LayoutNode<'_>> {
        self.inner.layout_node()
    }

    fn layout_children_mut(&mut self) -> Vec<&mut crate::component::DynComponent> {
        self.inner.layout_children_mut()
    }
}

/// `stack.ts:90-94`.
pub fn visible_stack_entries<'a>(
    entries: &'a [(&'a dyn Component, StackEntryOptions)],
    viewport: LayoutViewport,
) -> Vec<&'a (&'a dyn Component, StackEntryOptions)> {
    entries
        .iter()
        .filter(|(_, options)| options.is_visible(viewport))
        .collect()
}

/// `stack.ts:96-101`.
fn clamp_size(size: usize, entry: &StackEntryOptions) -> usize {
    let min = entry.min_size.unwrap_or(0);
    let max = entry.max_size.unwrap_or(usize::MAX).max(min);
    size.clamp(min, max)
}

/// `stack.ts:103-141`.
fn distribute(
    sizes: &mut [usize],
    entries: &[(&dyn Component, StackEntryOptions)],
    amount: usize,
    grow: bool,
) {
    let mut remaining = amount;
    while remaining > 0 {
        // pi filters candidates while keeping their index (`stack.ts:110-120`).
        let mut selected: Vec<usize> = Vec::new();
        for (index, (_, entry)) in entries.iter().enumerate() {
            let ok = if grow {
                entry.grow.unwrap_or(0) > 0 && sizes[index] < entry.max_size.unwrap_or(usize::MAX)
            } else {
                entry.shrink.unwrap_or(1) > 0 && sizes[index] > entry.min_size.unwrap_or(0)
            };
            if ok {
                selected.push(index);
            }
        }
        if selected.is_empty() {
            return;
        }
        let total_weight: usize = selected
            .iter()
            .map(|index| {
                let entry = &entries[*index].1;
                if grow {
                    entry.grow.unwrap_or(0)
                } else {
                    entry.shrink.unwrap_or(1) * sizes[*index].max(1)
                }
            })
            .sum();
        if total_weight == 0 {
            return;
        }
        let mut distributed = 0usize;
        for index in selected {
            if remaining == 0 {
                break;
            }
            let entry = &entries[index].1;
            let weight = if grow {
                entry.grow.unwrap_or(0)
            } else {
                entry.shrink.unwrap_or(1) * sizes[index].max(1)
            };
            let proposed = ((remaining * weight) / total_weight).max(1);
            let capacity = if grow {
                entry.max_size.unwrap_or(usize::MAX) - sizes[index]
            } else {
                sizes[index] - entry.min_size.unwrap_or(0)
            };
            let delta = remaining.min(proposed).min(capacity);
            if delta == 0 {
                continue;
            }
            if grow {
                sizes[index] += delta;
            } else {
                sizes[index] -= delta;
            }
            remaining -= delta;
            distributed += delta;
        }
        if distributed == 0 {
            return;
        }
    }
}

/// `stack.ts:143-157`.
pub fn allocate_stack_sizes(
    entries: &[(&dyn Component, StackEntryOptions)],
    intrinsic_sizes: &[usize],
    available_size: Option<usize>,
    gap: usize,
) -> Vec<usize> {
    let mut sizes: Vec<usize> = entries
        .iter()
        .enumerate()
        .map(|(index, (_, entry))| {
            let basis = entry
                .basis
                .unwrap_or_else(|| intrinsic_sizes.get(index).copied().unwrap_or(0));
            clamp_size(basis, entry)
        })
        .collect();
    let Some(available_size) = available_size else {
        return sizes;
    };
    let content_size = available_size.saturating_sub(entries.len().saturating_sub(1) * gap);
    let total: usize = sizes.iter().sum();
    if total < content_size {
        distribute(&mut sizes, entries, content_size - total, true);
    } else if total > content_size {
        distribute(&mut sizes, entries, total - content_size, false);
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(usize);

    impl Component for Fixed {
        fn render(&self, _width: usize) -> Vec<String> {
            vec![String::new(); self.0]
        }
    }

    fn component() -> &'static dyn Component {
        static EMPTY: &Fixed = &Fixed(0);
        EMPTY
    }

    #[test]
    fn auto_basis_uses_the_intrinsic_size() {
        let entries = vec![(component(), StackEntryOptions::default())];
        assert_eq!(allocate_stack_sizes(&entries, &[4], Some(10), 0), vec![4]);
    }

    #[test]
    fn fixed_basis_ignores_the_intrinsic_size() {
        let entries = vec![(
            component(),
            StackEntryOptions {
                basis: Some(2),
                ..Default::default()
            },
        )];
        assert_eq!(allocate_stack_sizes(&entries, &[4], Some(10), 0), vec![2]);
    }

    #[test]
    fn grow_entries_absorb_the_slack() {
        let entries = vec![
            (component(), StackEntryOptions::default()),
            (
                component(),
                StackEntryOptions {
                    grow: Some(1),
                    ..Default::default()
                },
            ),
        ];
        assert_eq!(
            allocate_stack_sizes(&entries, &[2, 2], Some(10), 0),
            vec![2, 8]
        );
    }

    #[test]
    fn grow_weights_are_proportional() {
        let entries = vec![
            (
                component(),
                StackEntryOptions {
                    grow: Some(1),
                    ..Default::default()
                },
            ),
            (
                component(),
                StackEntryOptions {
                    grow: Some(3),
                    ..Default::default()
                },
            ),
        ];
        // pi refines in a loop (`stack.ts:106-138`), so the split settles at
        // 3/5 rather than the first-pass 2/6.
        assert_eq!(
            allocate_stack_sizes(&entries, &[0, 0], Some(8), 0),
            vec![3, 5]
        );
    }

    #[test]
    fn gaps_are_reserved_before_distribution() {
        let entries = vec![
            (
                component(),
                StackEntryOptions {
                    grow: Some(1),
                    ..Default::default()
                },
            ),
            (
                component(),
                StackEntryOptions {
                    grow: Some(1),
                    ..Default::default()
                },
            ),
        ];
        assert_eq!(
            allocate_stack_sizes(&entries, &[0, 0], Some(11), 3),
            vec![5, 3]
        );
    }

    #[test]
    fn shrink_uses_size_weighted_factors_and_min_size() {
        let entries = vec![
            (component(), StackEntryOptions::default()),
            (
                component(),
                StackEntryOptions {
                    min_size: Some(3),
                    ..Default::default()
                },
            ),
        ];
        // Shrink weights are `shrink * max(1, size)`, so the second entry
        // gives up less and lands on its floor of 3 after refinement.
        assert_eq!(
            allocate_stack_sizes(&entries, &[6, 6], Some(6), 0),
            vec![2, 4]
        );
    }

    #[test]
    fn max_size_caps_growth() {
        let entries = vec![
            (
                component(),
                StackEntryOptions {
                    grow: Some(1),
                    max_size: Some(3),
                    ..Default::default()
                },
            ),
            (
                component(),
                StackEntryOptions {
                    grow: Some(1),
                    ..Default::default()
                },
            ),
        ];
        assert_eq!(
            allocate_stack_sizes(&entries, &[0, 0], Some(10), 0),
            vec![3, 7]
        );
    }

    #[test]
    fn without_an_available_size_nothing_is_distributed() {
        let entries = vec![
            (component(), StackEntryOptions::default()),
            (
                component(),
                StackEntryOptions {
                    grow: Some(1),
                    ..Default::default()
                },
            ),
        ];
        assert_eq!(allocate_stack_sizes(&entries, &[2, 2], None, 0), vec![2, 2]);
    }

    #[test]
    fn stacks_render_as_vertical_concatenation() {
        let mut stack = VStack::new(vec![]);
        stack.add_child(Box::new(Fixed(2)), StackEntryOptions::default());
        stack.add_child(Box::new(Fixed(1)), StackEntryOptions::default());
        assert_eq!(stack.render_lines(4).len(), 3);
        assert!(matches!(
            stack.layout_node(),
            Some(LayoutNode::VStack { .. })
        ));
    }

    #[test]
    fn visible_filtering_uses_the_viewport() {
        let entries = vec![(
            component(),
            StackEntryOptions {
                visible: Some(|viewport: &LayoutViewport| viewport.width > 10),
                ..Default::default()
            },
        )];
        assert_eq!(
            visible_stack_entries(
                &entries,
                LayoutViewport {
                    width: 20,
                    height: 1
                }
            )
            .len(),
            1
        );
        assert_eq!(
            visible_stack_entries(
                &entries,
                LayoutViewport {
                    width: 5,
                    height: 1
                }
            )
            .len(),
            0
        );
    }
}
