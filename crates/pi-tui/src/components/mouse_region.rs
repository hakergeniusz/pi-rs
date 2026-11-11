//! Adds mouse handling to an existing component without changing its rendering.
//! `packages/tui/src/components/mouse-region.ts:1-33`.

use crate::component::{Component, DynComponent};
use crate::mouse::{MouseEvent, MouseEventResult, dispatch_mouse_event};

/// `MouseRegionHandler`, `mouse-region.ts:7`.
pub type MouseRegionHandler = Box<dyn FnMut(&MouseEvent) -> Option<MouseEventResult> + Send>;

/// The child is asked first; its result wins. `mouse-region.ts:27-30`.
pub struct MouseRegion {
    child: Box<DynComponent>,
    on_mouse: MouseRegionHandler,
}

impl std::fmt::Debug for MouseRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MouseRegion").finish_non_exhaustive()
    }
}

impl MouseRegion {
    /// `mouse-region.ts:13-16`.
    pub fn new(child: Box<DynComponent>, on_mouse: MouseRegionHandler) -> Self {
        Self { child, on_mouse }
    }

    pub fn child(&self) -> &DynComponent {
        self.child.as_ref()
    }
}

impl Component for MouseRegion {
    fn render(&self, width: usize) -> Vec<String> {
        self.child.render(width)
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        let mut child =
            std::mem::replace(&mut self.child, Box::new(crate::components::spacer::Spacer));
        let dispatched = dispatch_mouse_event(child.as_mut(), event);
        self.child = child;
        let child_result = dispatched.map(|result| MouseEventResult {
            handled: Some(true),
            capture: result.capture,
            focus: result.focus,
            render: result.render,
            target: Some(result.target),
            focus_target: result.focus_target,
        });
        child_result.or_else(|| (self.on_mouse)(event))
    }

    // pi's `MouseRegion` defines exactly `render`, `handleMouse` and
    // `invalidate` (`mouse-region.ts:11-33`). It deliberately does *not*
    // forward `handleInput`, so `dispatchMouseEvent`'s focusTarget rewrite
    // (`mouse.rs:222-236`) must see `has_input_handler() == false` and leave
    // `focusTarget` as the child.
    fn invalidate(&mut self) {
        self.child.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::text::Text;
    use crate::mouse::MouseEventKind;

    struct Region {
        hits: std::sync::Mutex<Vec<i64>>,
    }

    impl Component for Region {
        fn render(&self, _width: usize) -> Vec<String> {
            vec!["child".to_string()]
        }

        fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
            self.hits.lock().expect("region").push(event.x);
            Some(MouseEventResult::handled())
        }
    }

    fn event(kind: MouseEventKind) -> MouseEvent {
        MouseEvent::new(kind, 0, 3, 0, 20, 1)
    }

    #[test]
    fn renders_the_child_unchanged() {
        let region = MouseRegion::new(
            Box::new(Text::new("hi", 0, 0)),
            Box::new(|_| Some(MouseEventResult::handled())),
        );
        assert_eq!(region.render(10), Text::new("hi", 0, 0).render(10));
    }

    #[test]
    fn child_result_wins_over_the_region_handler() {
        let region_probe = std::sync::Arc::new(Region {
            hits: std::sync::Mutex::new(Vec::new()),
        });
        let seen = region_probe.clone();
        let mut region = MouseRegion::new(
            Box::new(Region {
                hits: std::sync::Mutex::new(Vec::new()),
            }),
            Box::new(move |event| {
                seen.hits.lock().expect("region").push(event.x);
                Some(MouseEventResult::capture())
            }),
        );
        let result = region
            .handle_mouse(&event(MouseEventKind::Press))
            .expect("handled");
        assert_eq!(result.capture, None, "child result replaced the region's");
        assert!(region_probe.hits.lock().expect("region").is_empty());
    }

    #[test]
    fn region_handler_runs_when_the_child_ignores_the_event() {
        let mut region = MouseRegion::new(
            Box::new(Text::new("hi", 0, 0)),
            Box::new(|event| {
                Some(MouseEventResult {
                    handled: Some(true),
                    focus: Some(event.x == 3),
                    ..Default::default()
                })
            }),
        );
        let result = region
            .handle_mouse(&event(MouseEventKind::Press))
            .expect("handled");
        assert_eq!(result.focus, Some(true));
        assert!(
            result.target.is_none(),
            "a region handler is not a dispatch target"
        );
    }

    /// D1 regression: pi's `MouseRegion` has no `handleInput`
    /// (`mouse-region.ts:11-33`), so `dispatchMouseEvent`'s steal-prevention
    /// guard `forwarded.focus && component.handleInput` is falsy and
    /// `focusTarget` stays the child (`tui.ts:88-92`).
    #[test]
    fn a_wrapped_input_child_keeps_itself_as_the_focus_target() {
        struct Keyed {
            text: Text,
        }

        impl Component for Keyed {
            fn render(&self, width: usize) -> Vec<String> {
                self.text.render(width)
            }

            fn handle_mouse(&mut self, _event: &MouseEvent) -> Option<MouseEventResult> {
                Some(MouseEventResult::focus())
            }

            fn handle_input(&mut self, data: &str) -> Option<crate::component::ComponentMutation> {
                self.text.handle_input(data)
            }

            fn has_input_handler(&self) -> bool {
                true
            }
        }

        let child = Keyed {
            text: Text::new("abc", 0, 0),
        };
        let mut region = MouseRegion::new(Box::new(child), Box::new(|_| None));
        assert!(
            !region.has_input_handler(),
            "pi's MouseRegion declares no handleInput"
        );

        let result =
            dispatch_mouse_event(&mut region, &event(MouseEventKind::Press)).expect("dispatched");
        assert_eq!(result.focus, Some(true));
        let focus_target = result.focus_target.expect("a focus target was recorded");
        assert!(
            focus_target != std::ptr::from_ref(&region) as usize,
            "the wrapper stole the focus target; pi leaves it on the child"
        );
    }
}
