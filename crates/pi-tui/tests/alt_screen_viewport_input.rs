//! `handleViewportInput`, `tui-alt-screen.ts:671-786`.
//!
//! pi registers the alt screen's input listener in its constructor
//! (`:280`), so the bytes reach the viewport before any focused component.
//! The port is [`Tui::handle_input_at`], which is what
//! [`Tui::handle_input`] delegates to with a zeroed clock; the clock only
//! matters to the wheel accelerator and the double-click window.
//!
//! The keys are the raw byte sequences a terminal sends, not synthesised
//! key ids, so every assertion here goes through the same
//! `keys.rs` matcher a live terminal would.

use pi_tui::component::Component;
use pi_tui::terminal::{CaptureTerminal, TerminalSize};
use pi_tui::tui::{FocusTarget, OverlayAnchor, OverlayOptions, SizeValue, Tui, TuiStopOptions};
use pi_tui::tui_alt_screen::{AltScreenOptions, MultiplexerEnv};

// ---------------------------------------------------------------- harness

struct Alt {
    tui: Tui,
    width: usize,
    height: usize,
}

/// A component that renders exactly the lines it was built with, standing in
/// for the transcript document pi hangs off the implicit scroll view.
struct Lines(Vec<String>);

impl Component for Lines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.clone()
    }
}

impl Alt {
    fn new(size: (u16, u16)) -> Self {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: size.0,
            rows: size.1,
        });
        let mut tui = Tui::new(Box::new(terminal));
        // Injected so nothing reads the real process environment.
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
            width: size.0 as usize,
            height: size.1 as usize,
        }
    }

    fn transcript(&mut self, count: usize) {
        let lines = (0..count).map(|row| format!("line {row:04}")).collect();
        self.tui.add_child(Box::new(Lines(lines)));
        self.frame();
    }

    /// Lay the frame out so the primary scroll view is recorded, exactly as
    /// the last paint before an input event does in pi.
    fn frame(&mut self) {
        let _ = self.tui.alt_screen_layout_frame(self.width, self.height);
    }

    fn key(&mut self, data: &str) -> bool {
        self.tui.handle_input_at(data, 0)
    }

    fn top(&mut self) -> usize {
        self.tui.alt_screen_viewport_top()
    }

    fn stop(&mut self) {
        self.tui.stop(TuiStopOptions::default()).expect("stop");
    }
}

// ----------------------------------------------------------------- cases

/// The focus reports are consumed outright: `handleViewportInput`, `:673-693`.
#[test]
fn focus_reports_are_consumed() {
    let mut alt = Alt::new((20, 6));
    alt.transcript(100);
    assert!(alt.key("\x1b[I"), "FOCUS_IN is consumed");
    assert!(alt.key("\x1b[O"), "FOCUS_OUT is consumed");
    alt.stop();
}

/// `pageUp` moves the primary view by `viewportHeight - PAGE_SCROLL_OVERLAP`.
/// `:743-747`, with `PAGE_SCROLL_OVERLAP = 4` at `:75`.
#[test]
fn page_keys_scroll_the_primary_view_by_a_page() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(200);
    alt.tui.alt_screen_scroll_to_bottom();
    let at_end = alt.top();
    assert!(at_end > 0, "200 lines do not fit in ten rows");

    assert!(alt.key("\x1b[5~"), "pageUp is consumed");
    let after_up = alt.top();
    assert_eq!(at_end - after_up, 6, "ten rows minus the four-line overlap");

    assert!(alt.key("\x1b[6~"), "pageDown is consumed");
    assert_eq!(
        alt.top(),
        at_end,
        "the same page back again returns to the end"
    );
    alt.stop();
}

/// `home`/`end` (`tui.altScreen.top` / `bottom`), `:774-782`.
#[test]
fn home_and_end_jump_to_the_ends() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(200);
    alt.tui.alt_screen_scroll_to_bottom();
    assert!(alt.key("\x1b[H"), "home is consumed");
    assert_eq!(alt.top(), 0);
    assert!(alt.key("\x1b[F"), "end is consumed");
    assert!(alt.top() > 0, "end scrolls away from the top");
    alt.stop();
}

/// The viewport ids shadow the editor's own `pageUp`/`pageDown` in fullscreen
/// mode (`keybindings.ts:159`), so the transcript scrolls and the editor does
/// not see the key. The implicit document is the editor's container, so a
/// focus steal here would be visible as the view not moving.
#[test]
fn page_up_is_taken_by_the_viewport_not_the_focused_component() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(200);
    alt.tui.alt_screen_scroll_to_bottom();
    let at_end = alt.top();
    assert!(alt.key("\x1b[5~"));
    assert!(alt.top() < at_end);
    alt.stop();
}

/// `scrollToPrompt`, `:497-509`, over `OSC133_PROMPT_START` (`:74`). The
/// walk starts one line off the current offset and keeps the direction.
#[test]
fn ctrl_arrows_hop_between_shell_prompt_markers() {
    let mut alt = Alt::new((20, 10));
    let lines = (0..120)
        .map(|row| {
            if row % 20 == 0 {
                "\u{1b}]133;A\u{7}prompt".to_string()
            } else {
                format!("line {row:04}")
            }
        })
        .collect();
    alt.tui.add_child(Box::new(Lines(lines)));
    alt.frame();

    assert!(alt.key("\x1b[1;5A"), "ctrl+up is consumed");
    assert_eq!(alt.top(), 100, "the nearest prompt above the end");
    assert!(alt.key("\x1b[1;5A"), "ctrl+up again is consumed");
    assert_eq!(alt.top(), 80, "and the prompt above that one");
    assert!(alt.key("\x1b[1;5B"), "ctrl+down is consumed");
    assert_eq!(alt.top(), 100, "back down to the nearer prompt");
    // No prompt below the last one: the walk runs off the end and the offset
    // stays put, which is pi's silent return.
    alt.tui.alt_screen_scroll_to_bottom();
    assert_eq!(alt.top(), 110);
    assert!(alt.key("\x1b[1;5B"));
    assert_eq!(alt.top(), 110, "no marker below, no move");
    alt.stop();
}

/// The search keys are read before the defer rule, so they work while
/// another overlay holds focus. `:713-734`.
#[test]
fn the_search_key_opens_the_search_overlay_and_escape_closes_it() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(200);

    assert!(alt.key("\x1b[102;6u"), "ctrl+shift+f is consumed");
    assert!(alt.tui.search_is_open(), "the search overlay opened");
    assert_eq!(
        alt.tui
            .focused_target()
            .map(|target| matches!(target, FocusTarget::Overlay(_))),
        Some(true)
    );

    // Enter is `tui.altScreen.searchNext` (`keybindings.ts:193-196`), and it
    // is consumed without closing the search.
    assert!(alt.key("\r"));
    assert!(
        alt.tui.search_is_open(),
        "searchNext walks matches, it does not close"
    );

    assert!(alt.key("\x1b"), "escape is searchClose");
    assert!(!alt.tui.search_is_open());
    alt.stop();
}

/// `shouldDeferViewportInputToOverlay`, `:665-667`: an overlay that is not
/// the search overlay takes the viewport keys back.
#[test]
fn viewport_keys_are_deferred_to_a_focused_overlay() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(200);
    let overlay = alt.tui.show_overlay(
        Box::new(Lines(vec!["menu".to_string()])),
        Some(OverlayOptions {
            anchor: OverlayAnchor::Center,
            width: Some(SizeValue::Cells(10)),
            ..Default::default()
        }),
    );
    assert_eq!(
        alt.tui.focused_target(),
        Some(FocusTarget::Overlay(overlay))
    );
    alt.tui.alt_screen_scroll_to_bottom();
    let at_end = alt.top();

    assert!(!alt.key("\x1b[5~"), "pageUp falls through to the overlay");
    assert_eq!(alt.top(), at_end, "and the transcript did not move");
    assert!(!alt.key("\x1b[H"), "home falls through too");
    assert_eq!(alt.top(), at_end);

    // The search key is still the viewport's, before the defer rule.
    assert!(alt.key("\x1b[102;6u"));
    assert!(alt.tui.search_is_open());
    alt.stop();
}

/// The wheel report is read before the SGR press report, because a wheel
/// report is also a well-formed SGR report. `:695-712`.
#[test]
fn a_wheel_report_scrolls_the_view_instead_of_pressing() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(200);
    alt.tui.alt_screen_scroll_to_bottom();
    let at_end = alt.top();

    // Button 64 is pi's wheel-up code (`tui-alt-screen.ts:801-823`).
    assert!(alt.key("\x1b[<64;5;5M"), "the wheel report is consumed");
    assert!(alt.top() < at_end, "the wheel scrolled the transcript up");
    alt.stop();
}

/// `isMouseSequence`, `:696` — an X10 report the decoders do not recognise is
/// still swallowed so it never reaches a component as text.
#[test]
fn an_undecoded_mouse_report_is_swallowed() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(40);
    assert!(
        alt.key("\x1b[M\x20\x21\x21"),
        "a six-byte X10 report is consumed"
    );
    alt.stop();
}

/// A key the viewport does not bind falls through to the focused component,
/// so typing still reaches the editor. `handleInput`, `:783-786`.
#[test]
fn an_unbound_key_reaches_the_focused_component() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(40);
    assert!(!alt.key("q"), "q is not a viewport key");
    alt.stop();
}

/// `flash`, `:654-656`, and `AltScreenFlashContainer`, `alt-screen-flash.ts`.
/// pi's `setTimeout` is the host clock here, so expiry runs off the input path.
#[test]
fn a_flash_expires_on_the_host_clock() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(40);
    // The copy error flash: five seconds, `COPY_ERROR_FLASH_DURATION_MS`.
    alt.tui.flash("Copy failed", Some(5000), 1_000);
    assert_eq!(alt.tui.alt_screen_flashes(), vec!["Copy failed"]);
    alt.tui.handle_input_at("a", 5_999);
    assert_eq!(
        alt.tui.alt_screen_flashes(),
        vec!["Copy failed"],
        "one millisecond early"
    );
    alt.tui.handle_input_at("b", 6_000);
    assert!(
        alt.tui.alt_screen_flashes().is_empty(),
        "the deadline is inclusive-end"
    );
    alt.stop();
}

/// `beforeTerminalStart` disposes the stack, `:344`.
#[test]
fn entering_the_alt_screen_clears_the_flash_stack() {
    let mut alt = Alt::new((20, 10));
    alt.transcript(40);
    alt.tui.flash("Copied!", None, 0);
    let env = MultiplexerEnv {
        tmux: false,
        zellij: false,
        sty: false,
        term: Some("xterm-256color".to_string()),
    };
    alt.tui
        .enter_alt_screen_with(AltScreenOptions::default(), env)
        .expect("re-enter");
    assert_eq!(alt.tui.alt_screen_flashes(), [] as [&str; 0]);
    alt.stop();
}

/// `refreshSearch`, `:586-655`: the hits are kept for the painter, and an
/// explicit navigation scrolls an off-screen match into view
/// (`revealSelection`, `:637-653`).
#[test]
fn a_search_match_off_screen_is_revealed() {
    let mut alt = Alt::new((20, 10));
    let lines = (0..120).map(|row| format!("line {row:04}")).collect();
    alt.tui.add_child(Box::new(Lines(lines)));
    alt.frame();

    alt.tui.toggle_search();
    alt.tui.update_search_query("line 0090");
    let frame = alt.tui.alt_screen_layout_frame(alt.width, alt.height);
    let (selected, revealed) = alt.tui.refresh_search(&frame).expect("a search is open");
    assert_eq!(selected, 0, "the only hit");
    assert!(revealed, "the view moved to show it");
    // Row 90, a third of the ten-row viewport above it: 90 - 3.
    assert_eq!(alt.top(), 87);

    // `searchNext` from the only hit wraps back to it, and the view is already
    // there, so nothing moves: `revealSelection` returns `scrollTop !== before`.
    alt.tui.navigate_search(1);
    let frame = alt.tui.alt_screen_layout_frame(alt.width, alt.height);
    let (selected, revealed) = alt.tui.refresh_search(&frame).expect("a search is open");
    assert_eq!(selected, 0);
    assert!(!revealed, "already visible, so no scroll");
    assert_eq!(alt.top(), 87);
}

/// `applySearchHighlights`, `:1504-1561`: the hits are painted on the rows
/// the view shows, the selected one through the other hook, and ANSI runs
/// inside a span do not swallow the style.
#[test]
fn search_highlights_paint_the_visible_rows() {
    let terminal = CaptureTerminal::with_size(TerminalSize {
        columns: 20,
        rows: 4,
    });
    let mut tui = Tui::new(Box::new(terminal));
    let options = AltScreenOptions {
        search_match_style: std::sync::Arc::new(|text: &str| format!("[{text}]")),
        search_current_match_style: std::sync::Arc::new(|text: &str| format!("<{text}>")),
        ..AltScreenOptions::default()
    };
    let env = MultiplexerEnv {
        tmux: false,
        zellij: false,
        sty: false,
        term: Some("xterm-256color".to_string()),
    };
    tui.enter_alt_screen_with(options, env)
        .expect("enter alt screen");
    tui.start().expect("start");
    tui.add_child(Box::new(Lines(vec![
        "hit".to_string(),
        "\x1b[31mhit\x1b[39m".to_string(),
        "miss".to_string(),
        "hit".to_string(),
    ])));
    tui.toggle_search();
    tui.update_search_query("hit");
    let frame = tui.alt_screen_layout_frame(20, 4);
    tui.refresh_search(&frame);
    let frame = tui.alt_screen_layout_frame(20, 4);

    let painted = tui.alt_screen_apply_search_highlights(&frame.lines, &frame);
    assert_eq!(painted[0], "<hit>", "the first hit is the selected one");
    // The styled span keeps the colour codes and still gets the style applied
    // to each plain run around them.
    // The trailing reset sits outside the three matched columns, so the slice
    // leaves it behind — pi's `sliceByColumn(line, endCol, lineWidth - endCol)`
    // with nothing left to copy drops it too (`:1553`).
    assert_eq!(
        painted[1], "\x1b[31m[hit]",
        "a colour run does not swallow the style"
    );
    assert_eq!(painted[2], "miss", "a row without a hit is untouched");
    tui.stop(TuiStopOptions::default()).expect("stop");
}
