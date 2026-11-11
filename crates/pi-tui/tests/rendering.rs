//! Offline differential-rendering coverage.
//!
//! Everything here runs without a tty: the terminal is a
//! [`CaptureTerminal`], and assertions are made against the bytes written and
//! the cell grid built from them.

use std::sync::{Arc, Mutex};

use pi_tui::ansi::strip_terminal_sequences;
use pi_tui::component::{Component, ComponentMutation};
use pi_tui::frame::cell_grid;
use pi_tui::terminal::{CaptureTerminal, FrameCapture, TerminalSize};
use pi_tui::tui::{
    FocusTarget, OverlayAnchor, OverlayOptions, Redraw, RedrawInputs, SizeValue, Tui,
    TuiStopOptions, plain_lines, redraw_for,
};

struct Lines(Vec<String>);

impl Component for Lines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.clone()
    }
}

struct Echo(Vec<String>);

impl Component for Echo {
    fn render(&self, _width: usize) -> Vec<String> {
        vec![self.0.join("")]
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        self.0.push(data.to_string());
        Some(ComponentMutation { consume: true })
    }
}

struct Fixture {
    tui: Tui,
    capture: FrameCapture,
}

impl Fixture {
    fn new(size: (u16, u16)) -> Self {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: size.0,
            rows: size.1,
        });
        let capture = terminal.capture();
        let mut tui = Tui::new(Box::new(terminal));
        tui.start().expect("start");
        Self { tui, capture }
    }

    fn with_lines(size: (u16, u16), lines: &[&str]) -> Self {
        let mut fixture = Self::new(size);
        fixture.set_lines(lines);
        fixture
    }

    fn set_lines(&mut self, lines: &[&str]) {
        while self.tui.children_len() > 0 {
            self.tui.remove_child(0);
        }
        self.tui.add_child(Box::new(Lines(
            lines.iter().map(std::string::ToString::to_string).collect(),
        )));
    }

    fn render(&mut self) {
        self.tui.render_now(false);
    }

    fn grid(&self) -> pi_tui::frame::CellGrid {
        cell_grid(self.tui.last_lines(), 20, 10)
    }

    fn text(&self) -> Vec<String> {
        plain_lines(self.tui.last_lines())
    }
}

#[test]
fn first_render_emits_synchronized_output_without_clearing() {
    let mut fixture = Fixture::with_lines((20, 10), &["alpha", "beta"]);
    fixture.render();
    let frame = fixture.capture.last_frame().expect("a frame");
    assert!(
        frame.starts_with("\x1b[?2026h"),
        "synchronized output begins first"
    );
    assert!(frame.ends_with("\x1b[?2026l"));
    assert!(
        !frame.contains("\x1b[2J"),
        "a first paint must not clear the screen"
    );
    assert_eq!(
        fixture
            .text()
            .iter()
            .map(|line| line.trim_end().to_string())
            .collect::<Vec<_>>(),
        vec!["alpha", "beta"],
        "pi does not pad lines it renders"
    );
}

#[test]
fn an_identical_frame_writes_no_bytes() {
    let mut fixture = Fixture::with_lines((20, 10), &["alpha"]);
    fixture.render();
    fixture.capture.clear();
    fixture.render();
    assert_eq!(fixture.capture.last_frame(), None);
    assert_eq!(fixture.tui.full_redraws(), 1);
}

#[test]
fn a_changed_line_is_erased_and_rewritten() {
    let mut fixture = Fixture::with_lines((20, 10), &["alpha", "beta"]);
    fixture.render();
    fixture.capture.clear();
    fixture.set_lines(&["alpha", "BETA"]);
    fixture.render();
    let frame = fixture.capture.last_frame().expect("a frame");
    assert!(
        frame.contains("\x1b[2K"),
        "the line is cleared before rewriting"
    );
    assert!(frame.contains("BETA"));
    assert!(!frame.contains("alpha"), "only the changed line is written");
    assert_eq!(
        fixture.tui.full_redraws(),
        1,
        "an in-place edit is not a redraw"
    );
}

#[test]
fn the_cell_grid_matches_what_the_terminal_shows() {
    let mut fixture = Fixture::with_lines((20, 10), &["\x1b[1malpha\x1b[0m", "beta"]);
    fixture.render();
    let grid = fixture.grid();
    assert_eq!(grid.width, 20);
    assert_eq!(grid.rows()[0], "alpha               ");
    assert_eq!(grid.cell(0, 0).unwrap().style, "\x1b[1m");
    assert!(
        !grid.cell(0, 1).unwrap().style.contains('['),
        "the reset ends the bold run, got {:?}",
        grid.cell(0, 1).unwrap().style
    );
}

#[test]
fn appended_lines_scroll_the_output() {
    let mut fixture = Fixture::with_lines((20, 4), &["one"]);
    fixture.render();
    fixture.capture.clear();
    fixture.set_lines(&["one", "two", "three"]);
    fixture.render();
    let frame = fixture.capture.last_frame().expect("a frame");
    assert!(frame.contains("two"));
    assert!(frame.contains("three"));
    assert!(frame.contains("\r\n"), "appended lines move down");
}

#[test]
fn deleted_lines_clear_the_tail() {
    let mut fixture = Fixture::with_lines((20, 6), &["one", "two", "three"]);
    fixture.render();
    fixture.capture.clear();
    fixture.set_lines(&["one"]);
    fixture.render();
    assert!(fixture.capture.last_frame().is_some());
    assert_eq!(fixture.text().len(), 1);
    assert_eq!(fixture.text()[0].trim_end(), "one");
}

#[test]
fn a_terminal_width_change_forces_a_full_redraw() {
    let mut fixture = Fixture::with_lines((20, 6), &["alpha"]);
    fixture.render();
    fixture.tui.terminal_mut().set_size(TerminalSize {
        columns: 40,
        rows: 6,
    });
    fixture.capture.clear();
    fixture.render();
    let frame = fixture.capture.last_frame().expect("a frame");
    assert!(frame.contains("\x1b[2J\x1b[H"));
    assert_eq!(fixture.tui.full_redraws(), 2);
}

#[test]
fn a_terminal_height_change_forces_a_full_redraw() {
    let mut fixture = Fixture::with_lines((20, 6), &["alpha"]);
    fixture.render();
    fixture.tui.terminal_mut().set_size(TerminalSize {
        columns: 20,
        rows: 8,
    });
    fixture.capture.clear();
    fixture.render();
    assert!(fixture.capture.last_frame().unwrap().contains("\x1b[2J"));
    assert_eq!(fixture.tui.full_redraws(), 2);
}

#[test]
fn a_too_wide_line_stops_the_tui_and_reports_the_error() {
    let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = errors.clone();
    let mut fixture = Fixture::with_lines((20, 6), &["ok"]);
    fixture
        .tui
        .set_render_error_handler(Box::new(move |message| {
            sink.lock().expect("lock").push(message);
        }));
    fixture.render();
    fixture.set_lines(&["this line is far too wide"]);
    fixture.render();
    let errors = errors.lock().expect("lock").clone();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("exceeds terminal width"),
        "{}",
        errors[0]
    );
}

#[test]
fn the_cursor_marker_moves_the_hardware_cursor() {
    struct Marker(usize);
    impl Component for Marker {
        fn render(&self, _width: usize) -> Vec<String> {
            let mut lines = vec!["first".to_string(), "second".to_string()];
            lines[self.0].push_str(pi_tui::ansi::CURSOR_MARKER);
            lines
        }
    }
    let terminal = CaptureTerminal::with_size(TerminalSize {
        columns: 20,
        rows: 6,
    });
    let capture = terminal.capture();
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().expect("start");
    tui.add_child(Box::new(Marker(1)));
    tui.render_now(false);
    assert!(
        capture.output().contains("\x1b[7G"),
        "column 7, after \"second\""
    );
    assert!(!tui.last_lines()[1].contains(pi_tui::ansi::CURSOR_MARKER));
}

#[test]
fn overlays_composite_over_the_base_frame() {
    let mut fixture = Fixture::with_lines((20, 6), &["0123456789", "base line"]);
    let id = fixture.tui.show_overlay(
        Box::new(Lines(vec!["OV".to_string()])),
        Some(OverlayOptions {
            width: Some(SizeValue::Cells(4)),
            anchor: OverlayAnchor::TopLeft,
            ..Default::default()
        }),
    );
    fixture.render();
    let bounds = fixture.tui.overlay_bounds(id).expect("bounds");
    assert_eq!(bounds.col, 0);
    assert_eq!(bounds.row, 0);
    assert_eq!(bounds.width, 4);
    // The overlay covers columns 0..4 and the base keeps its tail.
    assert_eq!(
        strip_terminal_sequences(&fixture.tui.last_lines()[0]).trim_end(),
        "OV  456789"
    );
    assert_eq!(
        strip_terminal_sequences(&fixture.tui.last_lines()[1]).trim_end(),
        "base line"
    );
}

#[test]
fn a_centered_overlay_sits_in_the_middle() {
    let mut fixture = Fixture::with_lines((20, 6), &["base"]);
    let id = fixture.tui.show_overlay(
        Box::new(Lines(vec!["OV".to_string()])),
        Some(OverlayOptions {
            width: Some(SizeValue::Cells(4)),
            ..Default::default()
        }),
    );
    fixture.render();
    let bounds = fixture.tui.overlay_bounds(id).expect("bounds");
    assert_eq!(bounds.row, 2, "vertically centered in six rows");
    assert_eq!(bounds.col, 8, "horizontally centered in twenty columns");
    assert_eq!(
        strip_terminal_sequences(&fixture.tui.last_lines()[2]).trim_end(),
        "        OV"
    );
}

#[test]
fn a_percentage_anchored_overlay_uses_the_terminal_size() {
    let mut fixture = Fixture::with_lines((20, 6), &["base"]);
    let id = fixture.tui.show_overlay(
        Box::new(Lines(vec!["OV".to_string()])),
        Some(OverlayOptions {
            width: Some(SizeValue::Cells(4)),
            col: Some(SizeValue::Percent(50.0)),
            ..Default::default()
        }),
    );
    fixture.render();
    assert_eq!(fixture.tui.overlay_bounds(id).expect("bounds").col, 8);
}

#[test]
fn a_max_height_overlay_is_truncated() {
    let mut fixture = Fixture::with_lines((20, 10), &["base"]);
    let id = fixture.tui.show_overlay(
        Box::new(Lines(vec!["a".into(), "b".into(), "c".into(), "d".into()])),
        Some(OverlayOptions {
            width: Some(SizeValue::Cells(4)),
            max_height: Some(SizeValue::Cells(2)),
            ..Default::default()
        }),
    );
    fixture.render();
    let bounds = fixture.tui.overlay_bounds(id).expect("bounds");
    assert_eq!(bounds.height, 2);
}

#[test]
fn a_visible_predicate_hides_the_overlay_on_small_terminals() {
    let mut fixture = Fixture::with_lines((20, 6), &["base"]);
    let id = fixture.tui.show_overlay(
        Box::new(Lines(vec!["OV".to_string()])),
        Some(OverlayOptions {
            width: Some(SizeValue::Cells(4)),
            visible: Some(std::sync::Arc::new(|columns: u16, _rows: u16| {
                columns >= 60
            })),
            ..Default::default()
        }),
    );
    fixture.render();
    assert!(fixture.tui.overlay_bounds(id).is_none());
    assert!(!fixture.text().iter().any(|line| line.contains("OV")));
}

#[test]
fn hiding_an_overlay_removes_it_from_the_frame() {
    let mut fixture = Fixture::with_lines((20, 6), &["base"]);
    let id = fixture
        .tui
        .show_overlay(Box::new(Lines(vec!["OV".to_string()])), None);
    fixture.render();
    assert!(fixture.text().iter().any(|line| line.contains("OV")));
    fixture.tui.set_overlay_hidden(id, true);
    fixture.render();
    assert!(!fixture.text().iter().any(|line| line.contains("OV")));
}

#[test]
fn an_overlay_takes_focus_and_returns_it_when_closed() {
    let terminal = CaptureTerminal::with_size(TerminalSize {
        columns: 20,
        rows: 6,
    });
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().expect("start");
    tui.add_child(Box::new(Echo(Vec::new())));
    tui.set_focus(Some(FocusTarget::Root(0)));
    let id = tui.show_overlay(Box::new(Echo(Vec::new())), None);
    assert!(tui.handle_input("x"), "the overlay consumes input");
    tui.hide_overlay(id);
    assert_eq!(tui.focused_component(), Some(0));
    assert!(tui.handle_input("y"));
}

#[test]
fn input_schedules_a_render_request() {
    let terminal = CaptureTerminal::with_size(TerminalSize {
        columns: 20,
        rows: 6,
    });
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().expect("start");
    tui.add_child(Box::new(Echo(Vec::new())));
    tui.set_focus(Some(FocusTarget::Root(0)));
    tui.render_now(false);
    assert!(!tui.render_requested());
    tui.handle_input("q");
    assert!(tui.render_requested());
    tui.render_now(false);
    assert!(!tui.render_requested());
}

#[test]
fn a_forced_render_rebuilds_every_line() {
    let mut fixture = Fixture::with_lines((20, 6), &["alpha"]);
    fixture.render();
    fixture.tui.request_render();
    fixture.render();
    assert_eq!(fixture.tui.full_redraws(), 1);
    fixture.tui.render_now(true);
    assert_eq!(
        fixture.tui.full_redraws(),
        2,
        "force ignores the cached frame"
    );
}

#[test]
fn stopping_moves_the_cursor_below_the_content() {
    let mut fixture = Fixture::with_lines((20, 6), &["one", "two"]);
    fixture.render();
    fixture.capture.clear();
    fixture.tui.stop(TuiStopOptions::default()).expect("stop");
    let output = fixture.capture.output();
    assert!(output.contains(' '), "pi clears the last line");
    assert!(
        output.ends_with("\r\n"),
        "the cursor ends below the content"
    );
    assert!(!fixture.tui.terminal().is_tty());
}

#[test]
fn preserving_the_screen_skips_the_trailing_movement() {
    let mut fixture = Fixture::with_lines((20, 6), &["one", "two"]);
    fixture.render();
    fixture.capture.clear();
    fixture
        .tui
        .stop(TuiStopOptions {
            preserve_screen: true,
        })
        .expect("stop");
    assert_eq!(fixture.capture.output(), "");
}

#[test]
fn a_stopped_tui_does_not_render() {
    let mut fixture = Fixture::with_lines((20, 6), &["one"]);
    fixture.render();
    fixture
        .tui
        .stop(TuiStopOptions {
            preserve_screen: true,
        })
        .expect("stop");
    fixture.capture.clear();
    fixture.render();
    assert_eq!(fixture.capture.last_frame(), None);
}

#[test]
fn invalidate_forces_the_next_frame_to_be_complete() {
    let mut fixture = Fixture::with_lines((20, 6), &["alpha"]);
    fixture.render();
    fixture.tui.invalidate();
    fixture.capture.clear();
    fixture.render();
    let frame = fixture.capture.last_frame().expect("a frame");
    assert!(frame.contains("alpha"));
    assert_eq!(fixture.tui.full_redraws(), 2);
}

#[test]
fn clear_on_shrink_repaints_when_the_frame_shrinks() {
    let mut fixture = Fixture::with_lines((20, 6), &["one", "two", "three"]);
    fixture.tui.set_clear_on_shrink(true);
    fixture.render();
    fixture.set_lines(&["one"]);
    fixture.render();
    assert_eq!(
        fixture.tui.full_redraws(),
        2,
        "the shrink triggers a clear repaint"
    );
}

#[test]
fn a_zero_line_frame_writes_only_the_synchronization_wrapper() {
    let mut fixture = Fixture::with_lines((20, 6), &[]);
    fixture.render();
    let frame = fixture.capture.last_frame().expect("a frame");
    assert_eq!(frame, "\x1b[?2026h\x1b[?2026l");
}

// --------------------------------------------------------------- C.5 resize

/// Records the width it was rendered at, so "the frame re-measured" is an
/// assertion and not an inference.
#[derive(Default)]
struct WidthProbe(Arc<Mutex<Vec<usize>>>);

impl WidthProbe {
    fn seen(&self) -> Vec<usize> {
        self.0.lock().expect("widths").clone()
    }
}

impl Component for WidthProbe {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.lock().expect("widths").push(width);
        vec![format!("{width} columns")]
    }
}

#[test]
fn a_resize_re_measures_and_repaints_the_whole_frame() {
    // pi answers a SIGWINCH with `requestRender()` (`tui.ts:919-922`) and the
    // main screen repaints because the width moved (`tui-main-screen.ts:340`).
    let probe = WidthProbe::default();
    let terminal = CaptureTerminal::with_size(TerminalSize {
        columns: 20,
        rows: 6,
    });
    let capture = terminal.capture();
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().expect("start");
    tui.add_child(Box::new(WidthProbe(Arc::clone(&probe.0))));
    tui.render_now(false);
    assert_eq!(tui.full_redraws(), 1);

    // The host applied the new size, then delivered the tick.
    tui.terminal_mut().override_size(TerminalSize {
        columns: 42,
        rows: 6,
    });
    let size = tui.apply_resize().expect("size");
    assert_eq!(
        size,
        TerminalSize {
            columns: 42,
            rows: 6
        }
    );
    assert!(tui.render_requested(), "the tick asks for a frame");
    capture.clear();
    tui.render_now(false);

    assert_eq!(probe.seen(), vec![20, 42], "the child was re-measured");
    let frame = capture.last_frame().expect("a frame");
    assert!(
        frame.contains("\x1b[2J"),
        "a width change clears before repainting"
    );
    assert!(frame.contains("42 columns"));
    assert_eq!(tui.full_redraws(), 2, "a width change is a full repaint");
}

#[test]
fn a_resize_leaves_the_render_cache_alone_when_nothing_moved() {
    let probe = WidthProbe::default();
    let terminal = CaptureTerminal::with_size(TerminalSize {
        columns: 20,
        rows: 6,
    });
    let capture = terminal.capture();
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().expect("start");
    tui.add_child(Box::new(WidthProbe(Arc::clone(&probe.0))));
    tui.render_now(false);
    let _ = tui.apply_resize().expect("size");
    capture.clear();
    tui.render_now(false);
    assert_eq!(
        capture.last_frame(),
        None,
        "an unchanged terminal writes nothing"
    );
    assert_eq!(
        tui.full_redraws(),
        1,
        "the cached frame is still the comparison base"
    );
}

#[test]
fn a_shrink_with_a_live_overlay_does_not_clear_the_screen() {
    // `tui-main-screen.ts:356`: an overlay needs the rows a clear would wipe,
    // so the clear-on-shrink rule is off while one is up.
    let mut fixture = Fixture::with_lines((20, 6), &["one", "two", "three"]);
    fixture.tui.set_clear_on_shrink(true);
    fixture.render();
    let id = fixture.tui.show_overlay(
        Box::new(Lines(vec!["OV".to_string()])),
        Some(OverlayOptions {
            width: Some(SizeValue::Cells(4)),
            ..Default::default()
        }),
    );
    fixture.render();
    let before = fixture.tui.full_redraws();
    fixture.set_lines(&[]);
    fixture.capture.clear();
    fixture.render();
    assert!(
        fixture.tui.overlay_bounds(id).is_some(),
        "the overlay is still up"
    );
    assert_eq!(
        fixture.tui.full_redraws(),
        before,
        "a shrink under an overlay is an incremental frame"
    );
    assert!(
        !fixture.capture.output().contains("\x1b[2J"),
        "the overlay's padding must survive"
    );
}

#[test]
fn a_shrink_without_an_overlay_clears_the_screen() {
    // `tui-main-screen.ts:356-359`.
    let mut fixture = Fixture::with_lines((20, 6), &["one", "two", "three"]);
    fixture.tui.set_clear_on_shrink(true);
    fixture.render();
    assert_eq!(fixture.tui.full_redraws(), 1);
    fixture.set_lines(&["one"]);
    fixture.capture.clear();
    fixture.render();
    let frame = fixture.capture.last_frame().expect("a frame");
    assert!(
        frame.contains("\x1b[2J"),
        "the rows the content left are cleared"
    );
    assert_eq!(fixture.tui.full_redraws(), 2);
}

/// The rule as a table, so every arm of `tui-main-screen.ts:330-360` is pinned
/// even where the renderer has no terminal to move.
#[test]
fn the_resize_decision_matches_pi_rule_for_rule() {
    let size = |columns, rows| TerminalSize { columns, rows };
    let base = RedrawInputs {
        previous_size: Some(size(20, 6)),
        size: size(20, 6),
        has_previous_lines: true,
        new_line_count: 3,
        max_lines_rendered: 3,
        clear_on_shrink: false,
        has_overlays: false,
        termux: false,
    };
    assert_eq!(redraw_for(base), Redraw::Incremental, "nothing changed");
    assert_eq!(
        redraw_for(RedrawInputs {
            size: size(24, 6),
            ..base
        }),
        Redraw::Full,
        "a width change rewraps"
    );
    assert_eq!(
        redraw_for(RedrawInputs {
            size: size(20, 12),
            ..base
        }),
        Redraw::Full,
        "a height change realigns the viewport"
    );
    assert_eq!(
        redraw_for(RedrawInputs {
            size: size(20, 12),
            termux: true,
            ..base
        }),
        Redraw::Incremental,
        "termux toggles its height on the software keyboard"
    );
    assert_eq!(
        redraw_for(RedrawInputs {
            previous_size: None,
            has_previous_lines: false,
            ..base
        }),
        Redraw::First,
        "a first frame paints without clearing"
    );
    let shrinking = RedrawInputs {
        new_line_count: 1,
        clear_on_shrink: true,
        ..base
    };
    assert_eq!(
        redraw_for(shrinking),
        Redraw::Full,
        "a shrink clears the empty rows"
    );
    assert_eq!(
        redraw_for(RedrawInputs {
            has_overlays: true,
            ..shrinking
        }),
        Redraw::Incremental,
        "but not under an overlay"
    );
    assert_eq!(
        redraw_for(RedrawInputs {
            clear_on_shrink: false,
            ..shrinking
        }),
        Redraw::Incremental,
        "and not when the rule is off"
    );
}
