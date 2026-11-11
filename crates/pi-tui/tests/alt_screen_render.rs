//! The alt-screen frame painter. `doRender`, `tui-alt-screen.ts:1671-1755`.
//!
//! The painter is the only thing that writes an alt-screen frame, so these
//! tests drive it through [`Tui::render_now`] over a [`CaptureTerminal`] and
//! read the bytes back. What is pinned here is the order of the composites
//! (search highlights, jump-to-end, overlays, selection, flashes), the row
//! diffing, and the escape wrapper.

use pi_tui::component::Component;
use pi_tui::terminal::{CaptureTerminal, FrameCapture, TerminalSize};
use pi_tui::terminal_image::{
    ImageProtocol, KittyEncodeOptions, KittyImageMetadata, TerminalCapabilities, TerminalEnv,
    delete_kitty_image, encode_kitty, register_kitty_image_metadata, reset_kitty_image_metadata,
    set_capabilities, set_capability_environment,
};
use pi_tui::tui::{OverlayAnchor, OverlayOptions, SizeValue, Tui, TuiStopOptions};
use pi_tui::tui_alt_screen::{AltScreenOptions, DISABLE_MOUSE, EXIT_ALT_SCREEN, MultiplexerEnv};

struct Lines(Vec<String>);

impl Component for Lines {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.clone()
    }
}

struct Alt {
    tui: Tui,
    /// Shares the `Tui`'s buffer, so reading it reads what the painter wrote.
    capture: FrameCapture,
}

impl Alt {
    fn lines(&mut self, count: usize) {
        self.tui.add_child(Box::new(Lines(
            (0..count).map(|row| format!("line {row:02}")).collect(),
        )));
    }

    /// Paint one frame and hand back exactly the bytes that frame wrote.
    fn render(&mut self) -> String {
        let mark = self.capture.output().len();
        self.tui.render_now(false);
        let all = self.capture.output();
        all[mark..].to_string()
    }

    fn stop(&mut self) {
        self.tui
            .stop(TuiStopOptions {
                preserve_screen: true,
            })
            .expect("stop");
    }
}

/// Build a `Tui` over a capture terminal, with a handle on the same bytes.
fn tui_with_capture(size: (u16, u16)) -> (Tui, FrameCapture) {
    let terminal = CaptureTerminal::with_size(TerminalSize {
        columns: size.0,
        rows: size.1,
    });
    let capture = terminal.capture();
    (Tui::new(Box::new(terminal)), capture)
}

/// An entered alt screen over a captured terminal. Nothing here reads the real
/// environment or opens a tty.
fn alt(size: (u16, u16)) -> Alt {
    let (mut tui, capture) = tui_with_capture(size);
    let env = MultiplexerEnv {
        tmux: false,
        zellij: false,
        sty: false,
        term: Some("xterm-256color".to_string()),
    };
    tui.enter_alt_screen_with(AltScreenOptions::default(), env)
        .expect("enter alt screen");
    tui.start().expect("start");
    Alt { tui, capture }
}

/// The visible text of one line of the frame the painter last wrote. The rows
/// carry the resets `applyLineResets` appends, which are not what these
/// assertions are about.
fn text(line: &str) -> String {
    pi_tui::ansi::strip_terminal_sequences(line)
}

#[test]
fn the_first_frame_is_a_full_redraw() {
    let mut alt = alt((20, 3));
    alt.lines(3);
    let frame = alt.render();
    assert!(
        frame.starts_with("\x1b[?2026h"),
        "synchronized output opens the frame: {frame:?}"
    );
    assert!(frame.contains("\x1b[2J"), "the first frame clears");
    assert!(
        frame.contains("\x1b[1;1H\x1b[2Kline 00"),
        "row one is written at its address"
    );
    assert!(
        frame.contains("\x1b[3;1H\x1b[2Kline 02"),
        "and the last row"
    );
    assert!(
        frame.ends_with("\x1b[?25l\x1b[?2026l"),
        "the cursor is hidden and the frame closes"
    );
    alt.stop();
}

#[test]
fn an_unchanged_frame_writes_no_rows() {
    let mut alt = alt((20, 3));
    alt.lines(3);
    alt.render();
    let frame = alt.render();
    assert!(
        !frame.contains("\x1b[2K"),
        "no row was rewritten: {frame:?}"
    );
    assert!(
        frame.ends_with("\x1b[?25l\x1b[?2026l"),
        "the wrapper is still written"
    );
    alt.stop();
}

#[test]
fn only_the_changed_row_is_written() {
    let mut alt = alt((20, 3));
    alt.lines(3);
    alt.render();
    alt.tui.add_child(Box::new(Lines(vec![
        "line 00".to_string(),
        "changed".to_string(),
        "line 02".to_string(),
    ])));
    let frame = alt.render();
    assert_eq!(
        frame.matches("\x1b[2K").count(),
        1,
        "one row only: {frame:?}"
    );
    assert!(
        frame.contains("\x1b[2;1H\x1b[2Kchanged"),
        "and it is the second one"
    );
    alt.stop();
}

#[test]
fn the_frame_follows_the_transcripts_end() {
    let mut alt = alt((20, 3));
    alt.lines(30);
    alt.render();
    let lines = alt
        .tui
        .alt_screen_lines()
        .iter()
        .map(|line| text(line))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "the frame is the terminal's height");
    assert_eq!(
        lines,
        vec!["line 27", "line 28", "line 29"],
        "`followEnd` shows the newest screenful"
    );
    alt.stop();
}

#[test]
fn shell_prompt_markers_are_stripped_from_the_frame() {
    let mut alt = alt((20, 2));
    alt.tui.add_child(Box::new(Lines(vec![
        "\x1b]133;A\x07\x1b]133;B\x07$ ls".to_string(),
        "out".to_string(),
    ])));
    alt.render();
    assert_eq!(
        text(&alt.tui.alt_screen_lines()[0]),
        "$ ls",
        "the zone prefix never reaches the terminal"
    );
    alt.stop();
}

#[test]
fn a_flash_is_composited_over_the_first_row() {
    let mut alt = alt((20, 2));
    alt.lines(2);
    alt.render();
    alt.tui.flash("Copied!", None, 0);
    alt.render();
    // `compositeFlashes`, `:1657-1669`: the stack's newest entry is row zero,
    // right-aligned against the right edge. pi's flash line is
    // `\x1b[7m Copied! \x1b[27m` (`alt-screen-flash.ts:48`), and
    // `sliceWithWidth` breaks at `endCol` (`utils.ts:1310`), so the closing
    // `27` is dropped and `compositeTuiLine`'s own reset closes it instead.
    let row = &alt.tui.alt_screen_lines()[0];
    assert!(row.contains("\x1b[7m Copied! "), "{row:?}");
    assert_eq!(
        text(row).trim_end(),
        "line 00     Copied!",
        "the line underneath survives: {row:?}"
    );
    alt.stop();
}

#[test]
fn the_jump_to_end_label_appears_only_when_scrolled_away_from_the_end() {
    let (mut tui, _capture) = tui_with_capture((20, 3));
    let env = MultiplexerEnv {
        tmux: false,
        zellij: false,
        sty: false,
        term: Some("xterm-256color".to_string()),
    };
    let options = AltScreenOptions {
        scroll_to_end_indicator: Some(Box::new(|| "Jump to latest".to_string())),
        ..Default::default()
    };
    tui.enter_alt_screen_with(options, env)
        .expect("enter alt screen");
    tui.start().expect("start");
    tui.add_child(Box::new(Lines(
        (0..40).map(|row| format!("line {row:02}")).collect(),
    )));
    tui.render_now(false);
    assert!(
        tui.scroll_to_end_indicator_rect().is_none(),
        "at the end there is nothing to jump to"
    );

    tui.handle_input_at("\x1b[5~", 0);
    tui.render_now(false);
    let rect = tui
        .scroll_to_end_indicator_rect()
        .expect("scrolled away from the end");
    assert_eq!(rect.row, 2, "the label sits on the last row");
    assert!(
        tui.alt_screen_lines()[2].contains("Jump to latest"),
        "{:?}",
        tui.alt_screen_lines()[2]
    );

    tui.stop(TuiStopOptions {
        preserve_screen: true,
    })
    .expect("stop");
}

#[test]
fn an_overlay_is_composited_over_the_frame() {
    let mut alt = alt((20, 3));
    alt.tui.add_child(Box::new(Lines(vec![
        "hello world".to_string(),
        "second row".to_string(),
        "third row".to_string(),
    ])));
    alt.tui.show_overlay(
        Box::new(Lines(vec!["menu".to_string()])),
        Some(OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Cells(6)),
            ..Default::default()
        }),
    );
    alt.render();
    let lines = alt
        .tui
        .alt_screen_lines()
        .iter()
        .map(|line| text(line))
        .collect::<Vec<_>>();
    assert_eq!(
        lines[0].trim_end(),
        "menu  world",
        "the overlay covers the first six columns: {lines:?}"
    );
    alt.stop();
}

/// `TuiAltScreen.beforeTerminalStop`/`afterTerminalStop`,
/// `tui-alt-screen.ts:369-419`, reached through pi's one `stop`
/// (`tui.ts:970-980`): mouse reporting goes off before the terminal stops and
/// the alt screen is left afterwards. A host that only calls `Tui::stop` gets
/// all of it.
#[test]
fn stop_leaves_the_terminal_the_way_the_alt_screen_needs_it() {
    let (mut tui, capture) = tui_with_capture((20, 3));
    let env = MultiplexerEnv {
        tmux: false,
        zellij: false,
        sty: false,
        term: Some("xterm-256color".to_string()),
    };
    tui.enter_alt_screen_with(AltScreenOptions::default(), env)
        .expect("enter alt screen");
    tui.start().expect("start");
    tui.add_child(Box::new(Lines(vec!["line 00".to_string()])));
    tui.render_now(false);

    let mark = capture.output().len();
    tui.stop(TuiStopOptions {
        preserve_screen: true,
    })
    .expect("stop");
    let tail = &capture.output()[mark..];

    assert!(
        tail.contains(DISABLE_MOUSE),
        "mouse reporting is switched off first: {tail:?}"
    );
    assert!(
        tail.contains(EXIT_ALT_SCREEN),
        "the alt screen is left: {tail:?}"
    );
    let (mouse_off, leave) = (tail.find(DISABLE_MOUSE), tail.find(EXIT_ALT_SCREEN));
    assert!(
        mouse_off < leave,
        "mouse reporting goes off before the alt screen is left: {tail:?}"
    );
}

#[test]
fn the_hardware_cursor_stays_hidden_when_no_component_marks_one() {
    let mut alt = alt((20, 3));
    alt.lines(3);
    // `getShowHardwareCursor()`, `:1741`: nothing in this tree emits a cursor
    // marker, so the painter leaves the cursor hidden and just closes the frame.
    let frame = alt.render();
    assert!(frame.ends_with("\x1b[?25l\x1b[?2026l"), "{frame:?}");
    alt.stop();
}

/// A document whose lines the test can replace between frames, which is how
/// the image tests swap one image for another without touching the scroll
/// position.
#[derive(Clone)]
struct Document(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl Document {
    fn new(lines: Vec<String>) -> Self {
        Document(std::sync::Arc::new(std::sync::Mutex::new(lines)))
    }

    fn set(&self, lines: Vec<String>) {
        *self.0.lock().expect("document lock") = lines;
    }
}

impl Component for Document {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.lock().expect("document lock").clone()
    }
}

/// Kitty capabilities, in this thread only — the capability cache is a
/// thread-local, so nothing leaks into another test.
fn kitty_capabilities() {
    reset_kitty_image_metadata();
    set_capabilities(TerminalCapabilities {
        images: ImageProtocol::Kitty,
        true_color: true,
        hyperlinks: true,
    });
}

/// The environment `is_wezterm` reads, injected so no test touches the real
/// one.
fn environment_with_wezterm(wezterm: bool) -> TerminalEnv {
    let mut env = TerminalEnv::new();
    env.set("TERM", "xterm-256color");
    if wezterm {
        env.set("WEZTERM_PANE", "0");
    }
    set_capability_environment(Some(env.clone()));
    env
}

/// One Kitty line for `image_id`, with the metadata the placement parser
/// needs. Registering bumps the transmission generation, so calling this
/// twice with one id models a re-transmitted image.
fn kitty_line(image_id: u32, data: &str) -> String {
    kitty_line_sized(image_id, data, 100, 50)
}

fn kitty_line_sized(image_id: u32, data: &str, width_px: u32, height_px: u32) -> String {
    register_kitty_image_metadata(KittyImageMetadata {
        image_id,
        columns: 2,
        rows: 1,
        width_px,
        height_px,
    });
    encode_kitty(
        data,
        KittyEncodeOptions {
            columns: Some(2),
            rows: Some(1),
            image_id: Some(image_id),
            move_cursor: Some(false),
        },
    )
}

/// `fullRedraw` from `previousScreenWidth`/`previousScreenHeight`, `:1694-1695`:
/// a terminal that changed size clears and repaints every row, and the
/// counter pi exposes through `fullRedraws` moves with it (`:1709`).
#[test]
fn a_resize_between_frames_forces_a_full_redraw() {
    let mut alt = alt((20, 3));
    alt.lines(3);
    alt.render();
    let before = alt.tui.full_redraws();

    alt.tui.terminal_mut().set_size(TerminalSize {
        columns: 20,
        rows: 4,
    });
    alt.tui.add_child(Box::new(Lines(
        (0..4).map(|row| format!("row {row:02}")).collect(),
    )));
    let frame = alt.render();

    assert!(
        frame.contains("\x1b[2J"),
        "the resized frame clears: {frame:?}"
    );
    assert_eq!(
        frame.matches("\x1b[2K").count(),
        4,
        "every row is rewritten: {frame:?}"
    );
    assert_eq!(
        alt.tui.full_redraws(),
        before + 1,
        "and the redraw is counted"
    );

    let steady = alt.render();
    assert!(
        !steady.contains("\x1b[2J"),
        "an unchanged frame does not clear again: {steady:?}"
    );
    assert_eq!(
        alt.tui.full_redraws(),
        before + 1,
        "nor is it counted again"
    );
    alt.stop();
}

/// `clearRowsBeforeKittyImages`, `:1721-1731`: under `WezTerm` the rows a frame
/// touches are cleared in one pass and drawn in a second, because an erase
/// that lands after a placement wipes the image.
#[test]
fn a_wezterm_image_frame_clears_every_row_before_it_draws_any() {
    environment_with_wezterm(false);
    kitty_capabilities();
    let mut alt = alt((20, 2));
    let image_line = kitty_line(7, "AAAA");
    let document = Document::new(vec![format!("{image_line} one"), "after".to_string()]);
    alt.tui.add_child(Box::new(document.clone()));
    // Not WezTerm: the clear rides along with the row it belongs to.
    let plain = alt.render();
    assert!(
        plain.contains("\x1b[1;1H\x1b[2K\x1b_G"),
        "the row is cleared and drawn together: {plain:?}"
    );

    environment_with_wezterm(true);
    // Both rows change, so the frame touches both.
    document.set(vec![format!("{image_line} two"), "changed".to_string()]);
    let frame = alt.render();

    assert!(
        frame.contains("\x1b[1;1H\x1b[2K\x1b[2;1H\x1b[2K\x1b[1;1H"),
        "both rows are cleared before either is drawn: {frame:?}"
    );
    let clear_pass = frame.find("\x1b[1;1H\x1b[2K").expect("a clear pass");
    let draw_pass = frame.find("\x1b[1;1H\x1b_G").expect("a draw pass");
    assert!(
        clear_pass < draw_pass,
        "all the clears come before all the draws: {frame:?}"
    );
    assert_eq!(
        frame.matches("\x1b[2K").count(),
        2,
        "one clear per touched row, not two: {frame:?}"
    );
    alt.stop();
}

/// `prepareKittyScreen`, `:432-439`: a line whose image is already
/// transmitted with the same generation becomes its placement, so the
/// terminal gets no second copy of the payload.
#[test]
fn a_transmitted_kitty_image_is_replaced_by_its_placement() {
    environment_with_wezterm(false);
    kitty_capabilities();
    let mut alt = alt((20, 1));
    let line = kitty_line(11, "AAAA");
    let document = Document::new(vec![line.clone()]);
    alt.tui.add_child(Box::new(document.clone()));
    let first = alt.render();
    assert!(
        first.contains("\x1b_Ga=T,"),
        "the payload is sent once: {first:?}"
    );

    document.set(vec![format!("{line} tail")]);
    let frame = alt.render();
    assert!(
        frame.contains("\x1b_Ga=p,q=2,"),
        "the second frame places the image: {frame:?}"
    );
    assert!(
        !frame.contains("\x1b_Ga=a=T,"),
        "and does not send the payload again: {frame:?}"
    );
    assert!(
        frame.contains("tail"),
        "the text beside the image survives: {frame:?}"
    );
    alt.stop();
}

/// `:432-439`: the cache is keyed by image id and rewritten on every pass, so
/// a re-transmitted image is accounted for at its new size — and is evicted
/// on that basis once it scrolls out of view.
#[test]
fn a_retransmitted_kitty_image_refreshes_its_cache_entry() {
    environment_with_wezterm(false);
    kitty_capabilities();
    let mut alt = alt((20, 1));
    let document = Document::new(vec![kitty_line(13, "AAAA")]);
    alt.tui.add_child(Box::new(document.clone()));
    alt.render();

    // The same id, re-transmitted far larger: its decoded footprint
    // (`MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES`, `:79`) is now over the
    // whole budget.
    document.set(vec![kitty_line_sized(13, "BBBB", 20_000, 20_000)]);
    let retransmit = alt.render();
    assert!(
        retransmit.contains("BBBB"),
        "the new payload is transmitted: {retransmit:?}"
    );

    document.set(vec![kitty_line(14, "CCCC")]);
    let frame = alt.render();
    assert!(
        frame.contains(&delete_kitty_image(13)),
        "the refreshed entry is what the budget counts, so the big image goes: {frame:?}"
    );
    alt.stop();
}

/// `:445-468`: offscreen images are evicted, in insertion order, once the
/// cache budgets are exceeded, and an evicted image is transmitted again when
/// it comes back.
#[test]
fn the_kitty_cache_evicts_offscreen_images_and_retransmits_them() {
    environment_with_wezterm(false);
    kitty_capabilities();
    let mut alt = alt((20, 1));
    // Seventeen cached offscreen images exceed `MAX_CACHED_OFFSCREEN_KITTY_IMAGES`
    // (`:77`), so the first one inserted is the first one evicted.
    let lines: Vec<String> = (0..18)
        .map(|index| kitty_line(500 + index, "AAAA"))
        .collect();
    let document = Document::new(vec![lines[0].clone()]);
    alt.tui.add_child(Box::new(document.clone()));
    alt.render();

    let mut frames = String::new();
    for line in lines.iter().skip(1) {
        document.set(vec![line.clone()]);
        frames.push_str(&alt.render());
    }
    assert!(
        frames.contains(&delete_kitty_image(500)),
        "the oldest offscreen image is evicted: {frames:?}"
    );

    document.set(vec![lines[0].clone()]);
    let reentry = alt.render();
    assert!(
        reentry.contains("\x1b_Ga=T,"),
        "an evicted image is transmitted again: {reentry:?}"
    );
    alt.stop();
}
