//! Port of the four `mouse-components.test.ts` cases that need the
//! alt-screen renderer, plus the dispatch state machine around them.
//!
//! pi drives these through a `VirtualTerminal` and raw SGR bytes. Here the
//! terminal is a [`CaptureTerminal`] and the bytes go through the same two
//! decoders a host uses — [`pi_tui::stdin_buffer::StdinBuffer`] framing and
//! [`pi_tui::mouse::parse_sgr_mouse_event`] — so the raw report is exercised,
//! not a hand-built struct.
//!
//! The fourth case (`mouse-components.test.ts:228`, the settings submenu that
//! closes on click) is not here: pi gates it behind `enableSearch` and a
//! settings *submenu* host, which 4b never built. It is recorded in
//! `notes/phase5c/01-recon.md` with its `file:line`.

use std::sync::Arc;
use std::sync::Mutex;

use pi_tui::component::{Component, ComponentMutation, Container};
use pi_tui::components::editor::{Editor, EditorOptions, EditorTheme};
use pi_tui::components::input::Input;
use pi_tui::layout::LayoutFrame;
use pi_tui::mouse::{
    MouseEvent, MouseEventKind, MouseEventResult, SgrMouseEvent, is_mouse_sequence_bytes,
    parse_sgr_mouse_event,
};
use pi_tui::stdin_buffer::{StdinBuffer, StdinBufferOptions, StdinEvent};
use pi_tui::terminal::{CaptureTerminal, TerminalSize};
use pi_tui::tui::{FocusTarget, OverlayAnchor, OverlayOptions, SizeValue, Tui, TuiStopOptions};
use pi_tui::tui_alt_screen::{AltScreenOptions, MouseOutcome, MultiplexerEnv};

// ---------------------------------------------------------------- harness

/// A `TuiAltScreen` over an in-memory terminal, plus pi's `sendInput` +
/// `waitForRender` reduced to the two operations a synchronous test needs.
struct Alt {
    tui: Tui,
    stdin: StdinBuffer,
    width: usize,
    height: usize,
}

impl Alt {
    fn new(size: (u16, u16)) -> Self {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: size.0,
            rows: size.1,
        });
        let mut tui = Tui::new(Box::new(terminal));
        // No multiplexer in the test environment. The choice only decides the
        // bytes `beforeTerminalStart` writes, and `MultiplexerEnv` is injected
        // so nothing here reads the real process environment.
        let env = MultiplexerEnv {
            tmux: false,
            zellij: false,
            sty: false,
            term: Some("xterm-256color".to_string()),
        };
        tui.enter_alt_screen_with(AltScreenOptions::default(), env)
            .expect("enter alt screen");
        tui.start().expect("start");
        Self {
            tui,
            stdin: StdinBuffer::new(StdinBufferOptions::default()),
            width: size.0 as usize,
            height: size.1 as usize,
        }
    }

    fn frame(&mut self) -> LayoutFrame {
        self.tui.alt_screen_layout_frame(self.width, self.height)
    }

    /// pi's `waitForRender`: paint, then let the layout frame settle so the
    /// overlay rectangles the mouse pass hit-tests are the painted ones.
    fn render(&mut self) {
        self.tui.render_now(false);
        let _ = self.frame();
    }

    /// `terminal.sendInput(data)`: frame the bytes the way stdin delivers them,
    /// then run the alt screen's mouse precedence for each decoded report.
    fn send(&mut self, data: &str) -> bool {
        let mut out = Vec::new();
        self.stdin.process(data, &mut out);
        self.stdin.flush(&mut out);
        let mut consumed = false;
        for event in out {
            let StdinEvent::Data(chunk) = event else {
                self.tui.handle_input("");
                continue;
            };
            if is_mouse_sequence_bytes(chunk.as_bytes()) {
                let Some(raw) = parse_sgr_mouse_event(&chunk) else {
                    continue;
                };
                let frame = self.frame();
                if self.tui.handle_alt_mouse_event(&frame, &raw, 0) == MouseOutcome::Consumed {
                    consumed = true;
                }
                continue;
            }
            self.tui.handle_input(&chunk);
        }
        consumed
    }

    fn stop(&mut self) {
        self.tui
            .stop(TuiStopOptions {
                preserve_screen: true,
            })
            .expect("stop");
    }
}

/// pi's `terminal.sendInput("\x1b[<0;5;1M")` family, literals copied from pi's
/// test file. SGR reports are 1-based and 32 is pi's drag-motion code
/// (`tui-alt-screen.ts:801-823`).
mod sgr {
    pub fn press(x: u32, y: u32) -> String {
        format!("\x1b[<0;{x};{y}M")
    }
    pub fn release(x: u32, y: u32) -> String {
        format!("\x1b[<0;{x};{y}m")
    }
    pub fn drag(x: u32, y: u32) -> String {
        format!("\x1b[<32;{x};{y}M")
    }
}

// -------------------------------------------------------------- fixtures

/// Publishes the nested [`Input`]'s value so the test can read it back: Rust
/// has no downcast from the container's boxed child.
struct InputProbe {
    input: Input,
    value: Arc<Mutex<String>>,
}

impl Component for InputProbe {
    fn render(&self, width: usize) -> Vec<String> {
        self.input.render(width)
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        self.input.handle_key(data);
        *self.value.lock().expect("probe") = self.input.value().to_string();
        Some(ComponentMutation { consume: true })
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        self.input.handle_mouse(event)
    }

    fn has_input_handler(&self) -> bool {
        true
    }
}

/// pi's `InputOverlay` (`mouse-components.test.ts:49-56`): a `Container` whose
/// nested `Input` takes the keys, so the container keeps keyboard focus.
fn input_overlay(value: &str) -> (Container, Arc<Mutex<String>>) {
    let mut input = Input::with_prompt("");
    input.set_value(value);
    let published = Arc::new(Mutex::new(value.to_string()));
    let mut container = Container::new();
    container.add_child(Box::new(InputProbe {
        input,
        value: published.clone(),
    }));
    (container, published)
}

/// pi's `editorTheme` — an identity border keeps the assertions on text.
fn editor() -> Editor {
    Editor::new(
        EditorTheme {
            border_color: Arc::new(|text: &str| text.to_string()),
            ..EditorTheme::default()
        },
        EditorOptions::default(),
    )
}

/// Publishes the nested [`Editor`]'s text and cursor so the test can read them
/// back, and otherwise forwards pi's three `Editor` methods unchanged.
struct EditorProbe {
    editor: Editor,
    text: Arc<Mutex<String>>,
    cursor: Arc<Mutex<(usize, usize)>>,
}

impl EditorProbe {
    fn new(text: &str) -> Self {
        let mut editor = editor();
        editor.set_text(text);
        Self {
            editor,
            text: Arc::new(Mutex::new(text.to_string())),
            cursor: Arc::new(Mutex::new((0, 0))),
        }
    }

    fn publish(&mut self) {
        *self.text.lock().expect("probe") = self.editor.get_text();
        *self.cursor.lock().expect("probe") = self.editor.get_cursor();
    }
}

impl Component for EditorProbe {
    fn render(&self, width: usize) -> Vec<String> {
        self.editor.render(width)
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        self.editor.handle_input(data);
        self.publish();
        Some(ComponentMutation { consume: true })
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        let result = self.editor.handle_mouse(event);
        self.publish();
        result
    }

    fn has_input_handler(&self) -> bool {
        true
    }
}

// ----------------------------------------------------------------- cases

/// "keeps a delegating overlay focused when its nested input is clicked".
/// `mouse-components.test.ts:208-224`.
#[test]
fn keeps_a_delegating_overlay_focused_when_its_nested_input_is_clicked() {
    let mut alt = Alt::new((20, 4));
    let (overlay, value) = input_overlay("hi");
    let id = alt.tui.show_overlay(
        Box::new(overlay),
        Some(OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Cells(20)),
            ..OverlayOptions::default()
        }),
    );
    alt.render();

    alt.send(&sgr::press(5, 1));
    alt.send(&sgr::release(5, 1));
    alt.send("!");

    assert_eq!(*value.lock().expect("probe"), "hi!");
    assert_eq!(alt.tui.focused_target(), Some(FocusTarget::Overlay(id)));
    alt.stop();
}

/// "positions and focuses the multiline editor through alternate-screen
/// dispatch". `mouse-components.test.ts:261-280`.
#[test]
fn positions_and_focuses_the_multiline_editor_through_alt_screen_dispatch() {
    let mut alt = Alt::new((20, 6));
    let probe = EditorProbe::new("hello");
    let text = probe.text.clone();
    alt.tui.add_child(Box::new(probe));
    alt.render();

    alt.send(&sgr::press(3, 2));
    alt.send(&sgr::release(3, 2));
    alt.send("X");

    assert_eq!(*text.lock().expect("probe"), "heXllo");
    alt.stop();
}

/// The editor must end up holding the keyboard: `mouse-components.test.ts:279`.
#[test]
fn an_editor_click_moves_focus_onto_the_editor() {
    let mut alt = Alt::new((20, 6));
    let index = alt.tui.add_child(Box::new(EditorProbe::new("hello")));
    alt.tui.set_focus(Some(FocusTarget::Root(0)));
    alt.render();

    alt.send(&sgr::press(3, 2));
    alt.send(&sgr::release(3, 2));

    assert_eq!(alt.tui.focused_target(), Some(FocusTarget::Root(index)));
    alt.stop();
}

/// The click is what moves the cursor, and it lands on the clicked cell.
/// `mouse-components.test.ts:263-280` clicks `3;2`, which is column index 2
/// of the first body row — the `l` — so the next keystroke lands between the
/// two `l`s. pi asserts the same string.
#[test]
fn an_editor_click_positions_the_cursor_before_the_next_keystroke() {
    let mut alt = Alt::new((20, 6));
    let probe = EditorProbe::new("hello");
    let text = probe.text.clone();
    alt.tui.add_child(Box::new(probe));
    alt.render();

    // The click alone must not have typed anything.
    alt.send(&sgr::press(3, 2));
    alt.send(&sgr::release(3, 2));
    assert_eq!(*text.lock().expect("probe"), "hello");
    alt.send("X");
    assert_eq!(*text.lock().expect("probe"), "heXllo");
    alt.stop();
}

/// The editor leaves press, drag and release unhandled on purpose
/// (`components/editor.ts:653-657`) so the renderer can select across its rows.
/// A press that drags therefore never becomes a click: nothing is typed, and
/// nothing takes focus either.
#[test]
fn a_drag_across_the_editor_is_not_a_click() {
    let mut alt = Alt::new((20, 6));
    let probe = EditorProbe::new("hello");
    let text = probe.text.clone();
    alt.tui.add_child(Box::new(probe));
    alt.render();

    alt.send(&sgr::press(3, 2));
    alt.send(&sgr::drag(7, 2));
    alt.send(&sgr::release(7, 2));
    assert_eq!(*text.lock().expect("probe"), "hello");
    assert_eq!(alt.tui.focused_target(), None);
    alt.stop();
}

// ------------------------------------------------------------- dispatch

/// Records the kinds it sees, to pin the dispatch precedence.
struct KindProbe {
    seen: Arc<Mutex<Vec<MouseEventKind>>>,
}

impl Component for KindProbe {
    fn render(&self, _width: usize) -> Vec<String> {
        vec!["probe".to_string()]
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        self.seen.lock().expect("probe").push(event.kind);
        Some(MouseEventResult::handled())
    }
}

fn kind_probe() -> (KindProbe, Arc<Mutex<Vec<MouseEventKind>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    (KindProbe { seen: seen.clone() }, seen)
}

/// A press whose release lands elsewhere is a drag, never a click:
/// `tui-alt-screen.ts:896-926`.
#[test]
fn a_press_that_moves_is_never_a_click() {
    let mut alt = Alt::new((20, 6));
    let (probe, seen) = kind_probe();
    alt.tui.add_child(Box::new(probe));
    alt.render();

    alt.send(&sgr::press(2, 1));
    alt.send(&sgr::drag(9, 3));
    alt.send(&sgr::release(9, 3));

    assert_eq!(
        *seen.lock().expect("probe"),
        vec![
            MouseEventKind::Press,
            MouseEventKind::Drag,
            MouseEventKind::Release
        ],
        "a moved press never synthesises a click"
    );
    alt.stop();
}

/// A press and release on the same cell synthesises a click.
#[test]
fn a_press_and_release_on_one_cell_synthesises_a_click() {
    let mut alt = Alt::new((20, 6));
    let (probe, seen) = kind_probe();
    alt.tui.add_child(Box::new(probe));
    alt.render();

    alt.send(&sgr::press(2, 1));
    alt.send(&sgr::release(2, 1));

    assert_eq!(
        *seen.lock().expect("probe"),
        vec![
            MouseEventKind::Press,
            MouseEventKind::Release,
            MouseEventKind::Click
        ]
    );
    alt.stop();
}

struct ClickCountProbe {
    seen: Arc<Mutex<Vec<u32>>>,
}

impl Component for ClickCountProbe {
    fn render(&self, _width: usize) -> Vec<String> {
        vec!["probe".to_string()]
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        if event.kind == MouseEventKind::Click {
            self.seen
                .lock()
                .expect("probe")
                .push(event.click_count.unwrap_or(0));
        }
        Some(MouseEventResult::handled())
    }
}

/// `lastComponentClick`, `tui-alt-screen.ts:184-206`: two clicks in the
/// double-click window on the same cell count as 1 then 2.
#[test]
fn consecutive_clicks_on_one_cell_count_up() {
    let mut alt = Alt::new((20, 6));
    let seen = Arc::new(Mutex::new(Vec::new()));
    alt.tui
        .add_child(Box::new(ClickCountProbe { seen: seen.clone() }));
    alt.render();

    alt.send(&sgr::press(2, 1));
    alt.send(&sgr::release(2, 1));
    alt.send(&sgr::press(2, 1));
    alt.send(&sgr::release(2, 1));

    assert_eq!(*seen.lock().expect("probe"), vec![1, 2]);
    alt.stop();
}

/// Moving away from the pressed cell breaks the chain: `:919-920`.
#[test]
fn a_move_between_clicks_resets_the_click_count() {
    let mut alt = Alt::new((20, 6));
    let seen = Arc::new(Mutex::new(Vec::new()));
    alt.tui
        .add_child(Box::new(ClickCountProbe { seen: seen.clone() }));
    alt.render();

    // Two clicks on one cell: 1 then 2.
    alt.send(&sgr::press(2, 1));
    alt.send(&sgr::release(2, 1));
    alt.send(&sgr::press(2, 1));
    alt.send(&sgr::release(2, 1));
    // A drag away from the pressed cell produces no click and breaks the chain.
    alt.send(&sgr::press(2, 1));
    alt.send(&sgr::drag(7, 1));
    alt.send(&sgr::release(7, 1));
    // The next click on the original cell starts a new chain at 1.
    alt.send(&sgr::press(2, 1));
    alt.send(&sgr::release(2, 1));

    assert_eq!(*seen.lock().expect("probe"), vec![1, 2, 1]);
    alt.stop();
}

/// Renders a ten-column transcript line and records the click counts the
/// selection path synthesises.
struct WordClickProbe {
    seen: Arc<Mutex<Vec<u32>>>,
}

impl Component for WordClickProbe {
    fn render(&self, _width: usize) -> Vec<String> {
        vec!["alpha beta".to_string()]
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        // pi's `Editor` leaves press, drag and release unhandled
        // (`components/editor.ts:653-657`) so the selection path is the only
        // thing that produces the click; this probe does the same.
        if event.kind != MouseEventKind::Click {
            return None;
        }
        self.seen
            .lock()
            .expect("probe")
            .push(event.click_count.unwrap_or(0));
        Some(MouseEventResult::handled())
    }
}

/// `getClickCount`, `tui-alt-screen.ts:1232-1256`: the chain only continues
/// while the click resolves to the same *word*. A click past the end of the
/// line resolves to no word, and pi drops the remembered click there
/// (`:1253`, `lastClick = undefined`) instead of leaving it to be picked up by
/// the next word click, which would then count up from the stale value.
#[test]
fn a_click_past_the_line_breaks_the_word_click_chain() {
    let mut alt = Alt::new((20, 6));
    let seen = Arc::new(Mutex::new(Vec::new()));
    alt.tui
        .add_child(Box::new(WordClickProbe { seen: seen.clone() }));
    alt.render();

    // Twice on the word "alpha": 1 then 2.
    alt.send(&sgr::press(1, 1));
    alt.send(&sgr::release(1, 1));
    alt.send(&sgr::press(1, 1));
    alt.send(&sgr::release(1, 1));
    // Once at column 15, past the end of the ten-column line: no word.
    alt.send(&sgr::press(16, 1));
    alt.send(&sgr::release(16, 1));
    // Back on "alpha": the chain restarted instead of counting up to 3.
    alt.send(&sgr::press(1, 1));
    alt.send(&sgr::release(1, 1));

    assert_eq!(*seen.lock().expect("probe"), vec![1, 2, 1, 1]);
    alt.stop();
}

/// `!this.hasOverlay() && this.currentLayout`, `tui-alt-screen.ts:1372-1374`:
/// the press anchor normally resolves against the transcript's own scroll
/// view, so the selected text is read from the scroll view's *content*. An
/// overlay up turns that off, and the anchor becomes a screen coordinate, so
/// the text comes from `previousScreen` — the composited frame, overlay and
/// all.
#[test]
fn an_overlay_turns_the_press_anchor_back_into_screen_coordinates() {
    fn alt_with_overlay(with_overlay: bool) -> Alt {
        let mut alt = Alt::new((20, 3));
        alt.tui.add_child(Box::new(TranscriptRows {
            rows: ["alpha00", "beta01", "gamma02"]
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
        }));
        if with_overlay {
            alt.tui.show_overlay(
                Box::new(PlainLines {
                    lines: vec!["OVL".to_string()],
                }),
                Some(OverlayOptions {
                    anchor: OverlayAnchor::TopLeft,
                    width: Some(SizeValue::Cells(4)),
                    ..Default::default()
                }),
            );
        }
        alt.render();
        alt
    }

    fn drag_select(alt: &mut Alt) -> Option<String> {
        alt.send(&sgr::press(1, 1));
        alt.send(&sgr::drag(5, 1));
        alt.send(&sgr::release(5, 1));
        alt.tui
            .alt_screen_active_selection_text()
            .map(str::to_string)
    }

    let mut plain = alt_with_overlay(false);
    assert_eq!(
        drag_select(&mut plain).as_deref(),
        Some("alpha"),
        "the anchor followed the scroll view's content"
    );
    plain.stop();

    let mut overlaid = alt_with_overlay(true);
    assert_eq!(
        drag_select(&mut overlaid).as_deref(),
        Some("OVL a"),
        "with an overlay up the anchor is a screen coordinate, so the composited frame is the source"
    );
    overlaid.stop();
}

/// Renders the transcript rows the scroll view owns.
struct TranscriptRows {
    rows: Vec<String>,
}

impl Component for TranscriptRows {
    fn render(&self, _width: usize) -> Vec<String> {
        self.rows.clone()
    }
}

/// A single-line overlay that claims no mouse events, so the layout walk and
/// the selection path underneath still see every report.
struct PlainLines {
    lines: Vec<String>,
}

impl Component for PlainLines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }
}

/// An overlay above the tree claims the report before the layout walk:
/// `tui-alt-screen.ts:927-938`.
#[test]
fn an_overlay_hit_stops_the_layout_walk() {
    let mut alt = Alt::new((20, 6));
    let (under, under_seen) = kind_probe();
    alt.tui.add_child(Box::new(under));
    let (_, overlay_seen) = kind_probe();
    alt.tui.show_overlay(
        Box::new(KindProbe {
            seen: overlay_seen.clone(),
        }),
        Some(OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Cells(20)),
            ..OverlayOptions::default()
        }),
    );
    alt.render();

    alt.send(&sgr::press(2, 1));
    alt.send(&sgr::release(2, 1));

    assert_eq!(
        *overlay_seen.lock().expect("probe"),
        vec![
            MouseEventKind::Press,
            MouseEventKind::Release,
            MouseEventKind::Click
        ],
        "the topmost overlay owns the gesture"
    );
    assert!(
        under_seen.lock().expect("probe").is_empty(),
        "the layout walk underneath never ran: {:?}",
        under_seen.lock().expect("probe")
    );
    alt.stop();
}

/// A report below every box falls through to the host.
#[test]
fn a_report_outside_every_box_is_ignored() {
    let mut alt = Alt::new((20, 6));
    alt.render();
    let raw = SgrMouseEvent {
        button: 0,
        x: 999,
        y: 999,
        release: false,
    };
    let frame = alt.frame();
    assert_eq!(
        alt.tui.handle_alt_mouse_event(&frame, &raw, 0),
        MouseOutcome::Ignored
    );
    alt.stop();
}
