//! Mouse primitives.
//!
//! Ground truth: `packages/tui/src/tui.ts:20-116` (event/result/target types,
//! `dispatchMouseEvent`, `retargetMouseEvent`) and
//! `packages/tui/src/tui-alt-screen.ts:786-806` (`decodeMouseButton`,
//! `createMouseEvent`), `:845-850` (`applyMouseDispatchResult`),
//! `:867-880` (`getComponentClickCount`, the counters this file owns),
//! `:956-1008` (`parseWheelEvent`, `parseSgrMouseEvent`),
//! `:1629-1631` (`isMouseSequence`).
//!
//! The capture / press-target gesture state machine that *consumes* these
//! primitives (`mouseCapture`, `mousePressTarget`, `mousePressPoint`,
//! `mousePressMoved`, the release-time click synthesis and the
//! `lastComponentClick` reset on movement, `tui-alt-screen.ts:881-947`)
//! belongs to the renderer and lands with `tui_alt_screen.rs`.
//!
//! Coordinates are zero-based cells. `x`/`y` are local to the component that
//! receives the event; `screen_x`/`screen_y` are absolute terminal cells.

/// `TuiMouseEventType`. `tui.ts:21`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseEventKind {
    Press,
    Release,
    Move,
    Drag,
    Click,
    Wheel,
}

impl MouseEventKind {
    /// pi defaults `render` to false for `move`/`release` and true for the
    /// rest (`tui.ts:48-51`). `applyMouseDispatchResult` repeats the same
    /// rule (`tui-alt-screen.ts:855-858`).
    pub fn default_render(self) -> bool {
        !matches!(self, Self::Move | Self::Release)
    }
}

/// `TuiMouseButton`. `tui.ts:22`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// No button: motion and wheel reports carry none.
    None,
}

impl MouseButton {
    /// `decodeMouseButton`, `tui-alt-screen.ts:786-799`.
    pub fn from_raw(button: u32) -> Self {
        match button & 3 {
            0 => Self::Left,
            1 => Self::Middle,
            2 => Self::Right,
            _ => Self::None,
        }
    }

    /// pi's button name: `"left"`, `"middle"`, `"right"` or `"none"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
            Self::None => "none",
        }
    }
}

/// Stable identity for a component so a dispatch target can be compared and
/// re-dispatched to. pi compares `Component` references
/// (`tui.ts:91`, `:875`); the Rust port compares addresses, which is the same
/// identity with no lifetime plumbing.
pub type ComponentId = usize;

/// Identity of the component a dispatch landed on. `tui.ts:57-63`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseDispatchTarget {
    pub component: ComponentId,
    pub origin_x: i64,
    pub origin_y: i64,
    pub width: usize,
    pub height: usize,
}

/// Normalized mouse event. `tui.ts:25-42`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    pub button: MouseButton,
    /// Local to the receiving component.
    pub x: i64,
    pub y: i64,
    /// Absolute terminal coordinates.
    pub screen_x: i64,
    pub screen_y: i64,
    /// The receiving component's bounds.
    pub width: usize,
    pub height: usize,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// Logical lines; negative scrolls up. `tui.ts:40`.
    pub wheel_delta: Option<i64>,
    /// Consecutive click count, only for [`MouseEventKind::Click`].
    pub click_count: Option<u32>,
}

impl MouseEvent {
    /// `createMouseEvent`, `tui-alt-screen.ts:801-825`.
    pub fn new(
        kind: MouseEventKind,
        button: u32,
        x: i64,
        y: i64,
        width: usize,
        height: usize,
    ) -> Self {
        Self {
            kind,
            button: if kind == MouseEventKind::Wheel {
                MouseButton::None
            } else {
                MouseButton::from_raw(button)
            },
            x,
            y,
            screen_x: x,
            screen_y: y,
            width: width.max(1),
            height: height.max(1),
            shift: button & 4 != 0,
            alt: button & 8 != 0,
            ctrl: button & 16 != 0,
            wheel_delta: None,
            click_count: None,
        }
    }

    /// Builder: attach a wheel delta in logical lines.
    #[must_use]
    pub fn with_wheel_delta(mut self, delta: i64) -> Self {
        self.wheel_delta = Some(delta);
        self
    }

    /// Builder: attach a click count.
    #[must_use]
    pub fn with_click_count(mut self, count: u32) -> Self {
        self.click_count = Some(count);
        self
    }

    /// Builder: attach a click count that may be absent.
    #[must_use]
    pub fn with_optional_click_count(mut self, count: Option<u32>) -> Self {
        self.click_count = count;
        self
    }
}

/// What a component returns from `handle_mouse`. `tui.ts:44-53`.
///
/// `target` carries the forwarded-dispatch case: a container that dispatched
/// into a child returns the child's target so the caller redispatches there
/// (`tui.ts:88-92`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseEventResult {
    /// Stop propagation and suppress renderer-level fallback behaviour.
    pub handled: Option<bool>,
    /// Route subsequent drag/release events here. Implies handled.
    pub capture: Option<bool>,
    /// Give keyboard focus here. Implies handled.
    pub focus: Option<bool>,
    /// Explicit render request; `None` takes pi's per-kind default.
    pub render: Option<bool>,
    /// Set when this result was forwarded from a nested child.
    pub target: Option<MouseDispatchTarget>,
    /// Keyboard focus target, which may be a delegating parent container.
    pub focus_target: Option<ComponentId>,
}

impl MouseEventResult {
    /// Every flag unset: "not handled". `tui.ts:44-53`.
    pub const EMPTY: Self = MouseEventResult {
        handled: None,
        capture: None,
        focus: None,
        render: None,
        target: None,
        focus_target: None,
    };

    /// Handled, with every other flag unset.
    pub const fn handled() -> Self {
        Self {
            handled: Some(true),
            ..Self::EMPTY
        }
    }

    /// Not handled: the same as [`MouseEventResult::EMPTY`].
    pub const fn unhandled() -> Self {
        Self::EMPTY
    }

    /// Handled, with an explicit render request.
    pub fn handled_with_render(render: bool) -> Self {
        Self {
            handled: Some(true),
            render: Some(render),
            ..Self::EMPTY
        }
    }

    /// Handled, and asking for keyboard focus.
    pub fn focus() -> Self {
        Self {
            handled: Some(true),
            focus: Some(true),
            ..Self::EMPTY
        }
    }

    /// Handled, and capturing the drag until release.
    pub fn capture() -> Self {
        Self {
            handled: Some(true),
            capture: Some(true),
            ..Self::EMPTY
        }
    }

    /// Whether any of the handled, capture or focus flags is set.
    pub fn is_handled(&self) -> bool {
        self.handled.unwrap_or(false)
            || self.capture.unwrap_or(false)
            || self.focus.unwrap_or(false)
    }
}

/// A dispatch that actually reached a component. `tui.ts:72-79` —
/// `handled` is always true here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseDispatchResult {
    pub capture: Option<bool>,
    pub focus: Option<bool>,
    pub render: Option<bool>,
    pub target: MouseDispatchTarget,
    pub focus_target: Option<ComponentId>,
}

impl MouseDispatchResult {
    /// `applyMouseDispatchResult`'s return value (`tui-alt-screen.ts:845-859`).
    pub fn wants_render(&self, event: &MouseEvent, focus_changed: bool) -> bool {
        self.render
            .unwrap_or(focus_changed || event.kind.default_render())
    }
}

/// Identity of a component reference. Addresses are unique for the lifetime of
/// the borrow, which is all the dispatch bookkeeping needs.
pub fn component_id(component: &dyn crate::component::Component) -> ComponentId {
    std::ptr::from_ref(component).cast::<()>() as usize
}

/// `dispatchMouseEvent`, `tui.ts:81-104`.
pub fn dispatch_mouse_event(
    component: &mut dyn crate::component::Component,
    event: &MouseEvent,
) -> Option<MouseDispatchResult> {
    let id = component_id(&*component);
    let result = component.handle_mouse(event)?;
    if let Some(forwarded) = result.target {
        // The component dispatched into a child it hosts. Like a delegating
        // container it routes keys to that child itself, so it keeps keyboard
        // focus: focusing the child directly would leave focus on a component
        // the host is about to detach.
        let focus_target = if result.focus.unwrap_or(false) && component.has_input_handler() {
            Some(id)
        } else {
            result.focus_target
        };
        return Some(MouseDispatchResult {
            capture: result.capture,
            focus: result.focus,
            render: result.render,
            target: forwarded,
            focus_target,
        });
    }
    if !result.is_handled() {
        return None;
    }
    Some(MouseDispatchResult {
        capture: result.capture,
        focus: result.focus,
        render: result.render,
        target: MouseDispatchTarget {
            component: id,
            origin_x: event.screen_x - event.x,
            origin_y: event.screen_y - event.y,
            width: event.width,
            height: event.height,
        },
        focus_target: if result.focus.unwrap_or(false) {
            Some(id)
        } else {
            None
        },
    })
}

/// `retargetMouseEvent`, `tui.ts:106-116`.
pub fn retarget_mouse_event(event: &MouseEvent, target: &MouseDispatchTarget) -> MouseEvent {
    MouseEvent {
        x: event.screen_x - target.origin_x,
        y: event.screen_y - target.origin_y,
        width: target.width,
        height: target.height,
        ..*event
    }
}

/// Raw SGR mouse report. `SgrMouseEvent`, `tui-alt-screen.ts:958-964`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SgrMouseEvent {
    pub button: u32,
    pub x: i64,
    pub y: i64,
    pub release: bool,
}

/// Raw wheel report. `WheelEvent`, `tui-alt-screen.ts:945-951`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WheelEvent {
    /// `-1` scrolls up, `+1` scrolls down.
    pub direction: i64,
    pub x: i64,
    pub y: i64,
    /// The raw button code, which is `charCodeAt(3) - 32` and can therefore be
    /// negative for a malformed X10 report (`tui-alt-screen.ts:971`).
    pub button: i64,
}

/// `parseSgrMouseEvent`, `tui-alt-screen.ts:999-1008`.
///
/// Matches `/^\x1b\[<(\d+);(\d+);(\d+)([Mm])$/`.
pub fn parse_sgr_mouse_event(data: &str) -> Option<SgrMouseEvent> {
    let rest = data.strip_prefix("\x1b[<")?;
    let (body, release) = match rest.strip_suffix('M') {
        Some(body) => (body, false),
        None => (rest.strip_suffix('m')?, true),
    };
    let mut parts = body.split(';');
    let button = parts.next()?;
    let x = parts.next()?;
    let y = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Some(SgrMouseEvent {
        button: parse_digits(button)?,
        x: parse_digits(x)? as i64 - 1,
        y: parse_digits(y)? as i64 - 1,
        release,
    })
}

fn parse_digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `parseWheelEvent`, `tui-alt-screen.ts:956-983`. Handles both the SGR form
/// and the legacy 6-byte X10 form (`ESC [ M b x y`).
///
/// pi reads the X10 bytes with `charCodeAt`, so this takes bytes: an X10
/// report is raw terminal output and is not valid UTF-8 once the button or
/// coordinate carries the high bit.
pub fn parse_wheel_event_bytes(data: &[u8]) -> Option<WheelEvent> {
    if let Ok(text) = std::str::from_utf8(data)
        && let Some(sgr) = parse_sgr_mouse_event(text)
    {
        if sgr.button & 64 == 0 {
            return None;
        }
        let direction = sgr.button & 3;
        if direction != 0 && direction != 1 {
            return None;
        }
        return Some(WheelEvent {
            direction: if direction == 0 { -1 } else { 1 },
            x: sgr.x,
            y: sgr.y,
            button: i64::from(sgr.button),
        });
    }
    if data.len() == 6 && data.starts_with(b"\x1b[M") {
        // pi reads the bytes with `charCodeAt`, so a payload below 32/33
        // produces a *negative* code, and its `&` masks then operate on the
        // int32 the spec coerces to (`tui-alt-screen.ts:971-978`). The masks
        // are computed in `i32` so they match that coercion bit for bit;
        // `i64` would leave the high bits set and flip the tests.
        let button = i32::from(data[3]) - 32;
        if button & 64 == 0 {
            return None;
        }
        let direction = button & 3;
        if direction != 0 && direction != 1 {
            return None;
        }
        return Some(WheelEvent {
            direction: if direction == 0 { -1 } else { 1 },
            x: i64::from(data[4]) - 33,
            y: i64::from(data[5]) - 33,
            button: i64::from(button),
        });
    }
    None
}

/// `parseWheelEvent` over a UTF-8 slice. X10 reports with high-bit bytes must
/// go through [`parse_wheel_event_bytes`].
pub fn parse_wheel_event(data: &str) -> Option<WheelEvent> {
    parse_wheel_event_bytes(data.as_bytes())
}

/// `isMouseSequence`, `tui-alt-screen.ts:1629-1631`. Consume-only: an
/// unhandled mouse report must never fall through to the keyboard parser.
pub fn is_mouse_sequence_bytes(data: &[u8]) -> bool {
    if data.len() == 6 && data.starts_with(b"\x1b[M") {
        return true;
    }
    std::str::from_utf8(data).is_ok_and(|text| parse_sgr_mouse_event(text).is_some())
}

/// [`is_mouse_sequence_bytes`] over a UTF-8 slice.
pub fn is_mouse_sequence(data: &str) -> bool {
    parse_sgr_mouse_event(data).is_some() || (data.len() == 6 && data.starts_with("\x1b[M"))
}

/// `OSC133_PROMPT_START`, `tui-alt-screen.ts:74`:
/// `/^\x1b\]133;A(?:\x07|\x1b\\)/`.
///
/// The marker a shell writes when it starts a prompt, which is what
/// `scrollToPrompt` hops between.
pub fn is_osc133_prompt_start(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("\x1b]133;A") else {
        return false;
    };
    rest.starts_with('\x07') || rest.starts_with("\x1b\\")
}

/// `OSC133_ZONE_PREFIX`, `tui-alt-screen.ts:73`:
/// `/^(?:\x1b\]133;[ABC](?:\x07|\x1b\\))+/`.
///
/// The markers a shell writes around each command. They are meaningful to
/// `scroll_to_prompt`, which hops between the `A` ones, but the painter strips
/// them so the transcript does not show escape codes.
pub fn strip_osc133_zone_prefix(line: &str) -> String {
    let mut rest = line;
    loop {
        let Some(after_marker) = rest.strip_prefix("\x1b]133;") else {
            return rest.to_string();
        };
        let Some(kind_len) = after_marker
            .chars()
            .next()
            .filter(|kind| matches!(kind, 'A' | 'B' | 'C'))
            .map(char::len_utf8)
        else {
            return rest.to_string();
        };
        let terminator = &after_marker[kind_len..];
        rest = if let Some(stripped) = terminator.strip_prefix('\x07') {
            stripped
        } else if let Some(stripped) = terminator.strip_prefix("\x1b\\") {
            stripped
        } else {
            return rest.to_string();
        };
    }
}

/// Derive the event type from a raw report. `tui-alt-screen.ts:892-906`.
pub fn event_kind_for(raw: &SgrMouseEvent) -> MouseEventKind {
    if raw.release {
        MouseEventKind::Release
    } else if raw.button & 32 != 0 {
        if MouseButton::from_raw(raw.button) == MouseButton::None {
            MouseEventKind::Move
        } else {
            MouseEventKind::Drag
        }
    } else {
        MouseEventKind::Press
    }
}

/// `DOUBLE_CLICK_INTERVAL_MS`, `tui-alt-screen.ts:870-880`.
pub const DOUBLE_CLICK_INTERVAL_MS: u64 = 500;

/// The click counter behind `getComponentClickCount`. `tui-alt-screen.ts:869-880`.
#[derive(Debug, Default, Clone)]
pub struct ComponentClickTracker {
    last: Option<(u64, ComponentId, i64, i64, u32)>,
}

impl ComponentClickTracker {
    /// A tracker with no remembered click.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the last click, so the next one counts 1.
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// `getComponentClickCount`, `tui-alt-screen.ts:869-880`. Counts wrap at 3
    /// (`(count % 3) + 1`) and are keyed on component + exact cell.
    pub fn next(&mut self, component: ComponentId, x: i64, y: i64, now_ms: u64) -> u32 {
        let count = match self.last {
            Some((timestamp, previous, px, py, previous_count))
                if now_ms.saturating_sub(timestamp) <= DOUBLE_CLICK_INTERVAL_MS
                    && previous == component
                    && px == x
                    && py == y =>
            {
                (previous_count % 3) + 1
            }
            _ => 1,
        };
        self.last = Some((now_ms, component, x, y, count));
        count
    }
}

/// Single-slot double/triple click counter for text selection, exactly pi's
/// `lastClick`: only the immediately preceding click can extend a count, and a
/// click that misses a word wipes it. `tui-alt-screen.ts:1237-1261`.
#[derive(Debug, Default, Clone)]
pub struct SelectionClickTracker {
    last: Option<SelectionClick>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionClick {
    timestamp: u64,
    count: u32,
    row: usize,
    view: ComponentId,
    word_start: usize,
    word_end: usize,
}

impl SelectionClickTracker {
    /// A tracker with no remembered click.
    pub fn new() -> Self {
        Self::default()
    }

    /// `getClickCount`, `tui-alt-screen.ts:1237-1261`. `word` is `None` when
    /// the click landed on no word: the count is 1 and the previous click is
    /// cleared, matching `this.lastClick = word ? {...} : undefined`.
    pub fn next(
        &mut self,
        row: usize,
        view: ComponentId,
        word: Option<(usize, usize)>,
        now_ms: u64,
    ) -> u32 {
        let count = match (word, self.last) {
            (Some((word_start, word_end)), Some(previous))
                if now_ms.saturating_sub(previous.timestamp) <= DOUBLE_CLICK_INTERVAL_MS
                    && previous.row == row
                    && previous.view == view
                    && previous.word_start == word_start
                    && previous.word_end == word_end =>
            {
                (previous.count % 3) + 1
            }
            _ => 1,
        };
        self.last = word.map(|(word_start, word_end)| SelectionClick {
            timestamp: now_ms,
            count,
            row,
            view,
            word_start,
            word_end,
        });
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(kind: MouseEventKind, x: i64, y: i64) -> MouseEvent {
        MouseEvent::new(kind, 0, x, y, 40, 3)
    }

    struct Probe {
        result: Option<MouseEventResult>,
        seen: Vec<MouseEvent>,
        handles_input: bool,
    }

    impl crate::component::Component for Probe {
        fn render(&self, _width: usize) -> Vec<String> {
            Vec::new()
        }

        fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
            self.seen.push(*event);
            self.result
        }

        fn has_input_handler(&self) -> bool {
            self.handles_input
        }
    }

    #[test]
    fn decodes_buttons_and_modifier_bits() {
        assert_eq!(MouseButton::from_raw(0), MouseButton::Left);
        assert_eq!(MouseButton::from_raw(1), MouseButton::Middle);
        assert_eq!(MouseButton::from_raw(2), MouseButton::Right);
        assert_eq!(MouseButton::from_raw(3), MouseButton::None);
        let event = MouseEvent::new(MouseEventKind::Press, 4 | 8 | 16, 1, 2, 10, 10);
        assert!(event.shift && event.alt && event.ctrl);
        assert_eq!(event.button, MouseButton::Left);
    }

    #[test]
    fn wheel_forces_button_none() {
        let event = MouseEvent::new(MouseEventKind::Wheel, 0, 0, 0, 10, 10).with_wheel_delta(-3);
        assert_eq!(event.button, MouseButton::None);
        assert_eq!(event.wheel_delta, Some(-3));
    }

    #[test]
    fn render_defaults_follow_pi() {
        assert!(!MouseEventKind::Move.default_render());
        assert!(!MouseEventKind::Release.default_render());
        for kind in [
            MouseEventKind::Press,
            MouseEventKind::Click,
            MouseEventKind::Drag,
            MouseEventKind::Wheel,
        ] {
            assert!(kind.default_render(), "{kind:?}");
        }
    }

    #[test]
    fn parses_sgr_press_and_release() {
        assert_eq!(
            parse_sgr_mouse_event("\x1b[<0;5;1M"),
            Some(SgrMouseEvent {
                button: 0,
                x: 4,
                y: 0,
                release: false
            })
        );
        assert_eq!(
            parse_sgr_mouse_event("\x1b[<2;11;3m"),
            Some(SgrMouseEvent {
                button: 2,
                x: 10,
                y: 2,
                release: true
            })
        );
        assert_eq!(parse_sgr_mouse_event("\x1b[<0;5;1"), None);
        assert_eq!(parse_sgr_mouse_event("\x1b[<a;5;1M"), None);
        assert_eq!(parse_sgr_mouse_event("\x1b[<0;5;1;2M"), None);
    }

    #[test]
    fn parses_wheel_sgr_and_x10() {
        let wheel = parse_wheel_event("\x1b[<64;5;1M").expect("wheel");
        assert_eq!((wheel.direction, wheel.x, wheel.y), (-1, 4, 0));
        let down = parse_wheel_event("\x1b[<65;5;1M").expect("wheel");
        assert_eq!(down.direction, 1);
        // Alt+wheel keeps bit 3 set alongside the wheel bits.
        assert_eq!(
            parse_wheel_event("\x1b[<72;5;1M")
                .expect("alt wheel")
                .button
                & 8,
            8
        );
        // X10: ESC [ M b x y, button `byte - 32`, coords `byte - 33`.
        let x10 = parse_wheel_event_bytes(b"\x1b[M\x60\x41\x42").expect("x10 wheel");
        assert_eq!(x10.direction, -1);
        assert_eq!(x10.x, 65 - 33);
        assert_eq!(x10.y, 66 - 33);
        let x10_down = parse_wheel_event_bytes(b"\x1b[M\x61\x41\x42").expect("x10 wheel down");
        assert_eq!(x10_down.direction, 1);
        assert_eq!(parse_wheel_event("\x1b[<0;5;1M"), None, "not a wheel code");
        assert_eq!(
            parse_wheel_event_bytes(b"\x1b[Maaaaa"),
            None,
            "not a wheel code"
        );
        assert_eq!(
            parse_wheel_event_bytes(b"\x1b[M\x63\x41\x42"),
            None,
            "button 3 is not a wheel"
        );
    }

    /// F1 regression: pi reads the X10 payload with `charCodeAt`, so a byte
    /// below 32/33 yields a *negative* code and the `&` masks then read the
    /// two's-complement int32 (`tui-alt-screen.ts:971-978`). Unsigned
    /// subtraction panicked here in debug builds.
    #[test]
    fn x10_wheel_never_underflows_on_low_payload_bytes() {
        // Button byte 0x80: code 96, `96 & 64` passes, `96 & 3 == 0` is up.
        let high = parse_wheel_event_bytes(b"\x1b[M\x80\x00\x00").expect("high-bit button");
        assert_eq!(high.direction, -1);
        assert_eq!(high.button, 96);
        // Coordinates below 33 are negative in pi too; it returns them as-is.
        assert_eq!((high.x, high.y), (-33, -33));

        // Button byte 0: code -32. ToInt32(-32) is 0xFFFFFFE0, so `-32 & 64`
        // is 0x40, not 0: pi accepts it and `-32 & 3 == 0` reads as "up".
        let zero_button = parse_wheel_event_bytes(b"\x1b[M\x00\x41\x42").expect("code -32");
        assert_eq!(zero_button.direction, -1);
        assert_eq!(zero_button.button, -32);
        // Button byte 1: code -31, `-31 & 64 == 0x40` passes and
        // `-31 & 3 == 1` reads as "down", exactly as pi's int32 masks do.
        let negative = parse_wheel_event_bytes(b"\x1b[M\x01\x41\x42").expect("negative code");
        assert_eq!(negative.direction, 1);
        assert_eq!(negative.button, -31);
        // Button 0x61 with zeroed coordinates keeps the negative-x behaviour
        // without touching the button mask.
        let down = parse_wheel_event_bytes(b"\x1b[M\x61\x00\x00").expect("down");
        assert_eq!(down.direction, 1);
        assert_eq!((down.x, down.y), (-33, -33));
    }

    /// The Alt modifier is raw bit 3 of the button code, not a decoded field,
    /// because pi's Alt-wheel x5 reads it off the SGR code
    /// (`tui-alt-screen.ts:699-701`).
    #[test]
    fn wheel_button_code_keeps_the_alt_bit() {
        let sgr = parse_wheel_event("\x1b[<72;5;1M").expect("alt wheel");
        assert_eq!(sgr.button & 8, 8);
        let x10 = parse_wheel_event_bytes(b"\x1b[M\x68\x41\x42").expect("alt x10");
        assert_eq!(x10.button & 8, 8);
    }

    /// D2 regression: pi keeps a single `lastClick` and writes
    /// `this.lastClick = word ? {...} : undefined` (`tui-alt-screen.ts:1246-1255`),
    /// so a click that misses a word wipes the counter and only the
    /// immediately preceding click can extend a count.
    #[test]
    fn selection_click_counter_resets_on_a_non_word_click() {
        let mut tracker = SelectionClickTracker::new();
        assert_eq!(tracker.next(0, 1, Some((0, 4)), 0), 1);
        assert_eq!(tracker.next(0, 1, Some((0, 4)), 100), 2);
        // A click on no word at all clears the slot.
        assert_eq!(tracker.next(0, 1, None, 150), 1);
        assert_eq!(
            tracker.next(0, 1, Some((0, 4)), 200),
            1,
            "counter was wiped"
        );
        // A click on a different word replaces it rather than adding to it.
        assert_eq!(tracker.next(0, 1, Some((4, 8)), 250), 1);
        assert_eq!(tracker.next(0, 1, Some((0, 4)), 300), 1);
        // Same word again builds up, and wraps at three.
        assert_eq!(
            tracker.next(0, 1, Some((4, 8)), 350),
            1,
            "the (0,4) click replaced it"
        );
        assert_eq!(tracker.next(0, 1, Some((4, 8)), 400), 2);
        assert_eq!(tracker.next(0, 1, Some((4, 8)), 450), 3);
        assert_eq!(
            tracker.next(0, 1, Some((4, 8)), 500),
            1,
            "count wraps at three"
        );
        // Outside the interval, or on another row, it restarts.
        assert_eq!(
            tracker.next(0, 1, Some((4, 8)), 500 + DOUBLE_CLICK_INTERVAL_MS + 1),
            1
        );
        assert_eq!(tracker.next(1, 1, Some((4, 8)), 1000), 1);
        assert_eq!(
            tracker.next(1, 2, Some((4, 8)), 1100),
            1,
            "scroll view is part of the key"
        );
    }

    #[test]
    fn is_mouse_sequence_covers_both_encodings() {
        assert!(is_mouse_sequence("\x1b[<0;5;1M"));
        assert!(is_mouse_sequence_bytes(b"\x1b[M\x60\x41\x42"));
        assert!(!is_mouse_sequence("\x1b[A"));
        assert!(!is_mouse_sequence("x"));
    }

    #[test]
    fn derives_press_drag_move_and_release() {
        let press = SgrMouseEvent {
            button: 0,
            x: 0,
            y: 0,
            release: false,
        };
        assert_eq!(event_kind_for(&press), MouseEventKind::Press);
        let motion_left = SgrMouseEvent {
            button: 32,
            x: 0,
            y: 0,
            release: false,
        };
        assert_eq!(event_kind_for(&motion_left), MouseEventKind::Drag);
        let motion_none = SgrMouseEvent {
            button: 35,
            x: 0,
            y: 0,
            release: false,
        };
        assert_eq!(event_kind_for(&motion_none), MouseEventKind::Move);
        let release = SgrMouseEvent {
            button: 0,
            x: 0,
            y: 0,
            release: true,
        };
        assert_eq!(event_kind_for(&release), MouseEventKind::Release);
    }

    #[test]
    fn dispatch_records_target_origin_and_size() {
        let mut probe = Probe {
            result: Some(MouseEventResult::handled()),
            seen: Vec::new(),
            handles_input: false,
        };
        let event = mouse(MouseEventKind::Press, 2, 1);
        let mut event = event;
        event.screen_x = 7;
        event.screen_y = 9;
        event.width = 12;
        event.height = 4;
        let result = dispatch_mouse_event(&mut probe, &event).expect("dispatch");
        assert_eq!(result.target.origin_x, 5);
        assert_eq!(result.target.origin_y, 8);
        assert_eq!((result.target.width, result.target.height), (12, 4));
        assert_eq!(result.focus_target, None);
    }

    #[test]
    fn dispatch_drops_a_bare_unhandled_result() {
        let mut probe = Probe {
            result: Some(MouseEventResult::unhandled()),
            seen: Vec::new(),
            handles_input: false,
        };
        assert!(dispatch_mouse_event(&mut probe, &mouse(MouseEventKind::Move, 0, 0)).is_none());
    }

    #[test]
    fn dispatch_returns_none_when_the_component_ignores_the_event() {
        let mut probe = Probe {
            result: None,
            seen: Vec::new(),
            handles_input: false,
        };
        assert!(dispatch_mouse_event(&mut probe, &mouse(MouseEventKind::Move, 0, 0)).is_none());
    }

    #[test]
    fn dispatch_focuses_the_handling_component() {
        let mut probe = Probe {
            result: Some(MouseEventResult::focus()),
            seen: Vec::new(),
            handles_input: false,
        };
        let result = dispatch_mouse_event(&mut probe, &mouse(MouseEventKind::Press, 0, 0))
            .expect("dispatch");
        assert_eq!(result.focus, Some(true));
        assert_eq!(result.focus_target, Some(result.target.component));
    }

    #[test]
    fn delegating_component_keeps_focus_when_it_handles_input() {
        // mouse-components.test.ts:228-265 — a Container that forwarded into a
        // nested Input keeps keyboard focus itself.
        let child_target = MouseDispatchTarget {
            component: 7,
            origin_x: 0,
            origin_y: 0,
            width: 20,
            height: 1,
        };
        let mut probe = Probe {
            result: Some(MouseEventResult {
                handled: Some(true),
                focus: Some(true),
                target: Some(child_target),
                focus_target: Some(7),
                ..MouseEventResult::EMPTY
            }),
            seen: Vec::new(),
            handles_input: true,
        };
        let result = dispatch_mouse_event(&mut probe, &mouse(MouseEventKind::Press, 5, 0))
            .expect("dispatch");
        assert_eq!(result.target.component, 7);
        assert_ne!(result.focus_target, Some(7));
    }

    #[test]
    fn delegating_component_without_input_passes_focus_through() {
        let child_target = MouseDispatchTarget {
            component: 7,
            origin_x: 0,
            origin_y: 0,
            width: 20,
            height: 1,
        };
        let mut probe = Probe {
            result: Some(MouseEventResult {
                handled: Some(true),
                focus: Some(true),
                target: Some(child_target),
                focus_target: Some(7),
                ..MouseEventResult::EMPTY
            }),
            seen: Vec::new(),
            handles_input: false,
        };
        let result = dispatch_mouse_event(&mut probe, &mouse(MouseEventKind::Press, 5, 0))
            .expect("dispatch");
        assert_eq!(result.focus_target, Some(7));
    }

    #[test]
    fn retarget_recomputes_local_coordinates() {
        let mut event = mouse(MouseEventKind::Drag, 99, 99);
        event.screen_x = 30;
        event.screen_y = 12;
        let target = MouseDispatchTarget {
            component: 1,
            origin_x: 10,
            origin_y: 4,
            width: 25,
            height: 6,
        };
        let retargeted = retarget_mouse_event(&event, &target);
        assert_eq!((retargeted.x, retargeted.y), (20, 8));
        assert_eq!((retargeted.width, retargeted.height), (25, 6));
    }

    #[test]
    fn click_counter_wraps_at_three() {
        let mut tracker = ComponentClickTracker::new();
        assert_eq!(tracker.next(1, 4, 4, 0), 1);
        assert_eq!(tracker.next(1, 4, 4, 100), 2);
        assert_eq!(tracker.next(1, 4, 4, 200), 3);
        assert_eq!(tracker.next(1, 4, 4, 300), 1);
        // A different cell starts over.
        assert_eq!(tracker.next(1, 5, 4, 320), 1);
        // So does a different component.
        assert_eq!(tracker.next(2, 5, 4, 340), 1);
        // So does a late click.
        assert_eq!(tracker.next(2, 5, 4, 5_400), 1);
    }

    #[test]
    fn wants_render_defaults_to_the_kind() {
        let result = MouseDispatchResult {
            capture: None,
            focus: None,
            render: None,
            target: MouseDispatchTarget {
                component: 0,
                origin_x: 0,
                origin_y: 0,
                width: 1,
                height: 1,
            },
            focus_target: None,
        };
        assert!(!result.wants_render(&mouse(MouseEventKind::Move, 0, 0), false));
        assert!(!result.wants_render(&mouse(MouseEventKind::Release, 0, 0), false));
        assert!(result.wants_render(&mouse(MouseEventKind::Press, 0, 0), false));
        // A focus change forces a render even for a move.
        assert!(result.wants_render(&mouse(MouseEventKind::Move, 0, 0), true));
    }
}
