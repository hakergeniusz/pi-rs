//! pi's colour-scheme listener set (`tui.ts:943-948`, `:1167-1178`), and the
//! per-query late reply of the terminal-colour query (`tui.ts:1481-1487`).
//!
//! Everything runs on a [`CaptureTerminal`]: no `/dev/tty`, no process
//! environment, no timers. Deadlines are host-supplied instants handed to
//! [`Tui::service_terminal_queries`], so the tests drive them by value.

use std::sync::{Arc, Mutex};

use pi_tui::terminal::{CaptureTerminal, TerminalSize};
use pi_tui::terminal_colors::{RgbColor, TERMINAL_PALETTE_SIZE, TerminalColorScheme};
use pi_tui::tui::{
    ColorSchemeListener, ColorSchemeListenerId, ColorSchemeListenerRegistry, TerminalColors, Tui,
};

const DARK: &str = "\x1b[?997;1n";
const LIGHT: &str = "\x1b[?997;2n";
/// pi's `DA1` reply (`tui.ts:170`), the fence that ends a colour query.
const DA1: &str = "\x1b[?62;22c";

fn tui() -> Tui {
    let terminal = CaptureTerminal::with_size(TerminalSize {
        columns: 20,
        rows: 6,
    });
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().expect("start");
    tui
}

/// A listener that records one label per call into a shared log.
fn labelled(calls: &Arc<Mutex<Vec<&'static str>>>, name: &'static str) -> ColorSchemeListener {
    let sink = Arc::clone(calls);
    Arc::new(move |_scheme: TerminalColorScheme| sink.lock().expect("lock").push(name))
}

fn log(calls: &Arc<Mutex<Vec<&'static str>>>) -> Vec<&'static str> {
    calls.lock().expect("lock").clone()
}

/// pi's `Set.add` (`tui.ts:944`) dedupes an identical listener reference, and
/// the returned unsubscribe closure deletes that reference — so registering
/// one function twice keeps one entry and both registrations name it.
#[test]
fn registering_one_colour_scheme_listener_twice_keeps_one_entry() {
    let mut tui = tui();
    let registry = tui.color_scheme_listener_registry();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    let listener: ColorSchemeListener = labelled(&calls, "same");
    let first = tui.add_color_scheme_listener(Arc::clone(&listener));
    let second = tui.add_color_scheme_listener(Arc::clone(&listener));
    assert_eq!(
        first, second,
        "the second registration names the entry already there"
    );
    assert_eq!(
        registry.len(),
        1,
        "pi's Set holds one entry for one reference"
    );

    // A separately built closure is a different reference, so a second entry.
    tui.add_color_scheme_listener(labelled(&calls, "other"));
    assert_eq!(registry.len(), 2, "a distinct closure is a distinct entry");

    assert!(tui.consume_terminal_response(DARK));
    assert_eq!(log(&calls), vec!["same", "other"], "the duplicate ran once");

    // Both registrations named the one entry, so removing either removes it.
    tui.remove_color_scheme_listener(second);
    calls.lock().expect("lock").clear();
    tui.consume_terminal_response(DARK);
    assert_eq!(log(&calls), vec!["other"]);

    // pi's `Set.add` dedupes against what is *currently* in the set, so a
    // closure registered after its removal is a fresh entry with a fresh id.
    let again = tui.add_color_scheme_listener(Arc::clone(&listener));
    assert_ne!(again, first, "a removed listener's id is never reused");
    calls.lock().expect("lock").clear();
    tui.consume_terminal_response(DARK);
    assert_eq!(log(&calls), vec!["other", "same"]);
}

/// pi deletes from the same `Set` it iterates (`tui.ts:1174-1176`), so a
/// listener removed while the walk is running is gone for the rest of it, and
/// the entry that shifts into the freed slot must still be visited.
#[test]
fn a_listener_removed_mid_dispatch_does_not_skip_the_next_one() {
    let mut tui = tui();
    let registry: ColorSchemeListenerRegistry = tui.color_scheme_listener_registry();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    // The id only exists once the listener is registered, so the listener
    // reads its own id back out of a cell the host fills in afterwards.
    let mine: Arc<Mutex<Option<ColorSchemeListenerId>>> = Arc::new(Mutex::new(None));
    let cell = Arc::clone(&mine);
    let from_inside = registry.clone();
    let first_calls = Arc::clone(&calls);
    let id = tui.add_color_scheme_listener(Arc::new(move |_scheme: TerminalColorScheme| {
        first_calls.lock().expect("lock").push("a");
        if let Some(id) = cell.lock().expect("lock").take() {
            from_inside.remove(id);
        }
    }));
    *mine.lock().expect("lock") = Some(id);
    tui.add_color_scheme_listener(labelled(&calls, "b"));
    tui.add_color_scheme_listener(labelled(&calls, "c"));
    assert_eq!(
        registry.len(),
        3,
        "the registry counts what the TUI registered"
    );

    tui.consume_terminal_response(DARK);
    assert_eq!(
        log(&calls),
        vec!["a", "b", "c"],
        "b shifted into the slot a left"
    );

    calls.lock().expect("lock").clear();
    tui.consume_terminal_response(DARK);
    assert_eq!(log(&calls), vec!["b", "c"], "a is gone for good");
}

/// An entry deleted before the cursor reaches it is skipped: the walk is over
/// the live set, not a snapshot of it (`tui.ts:1174-1176`).
#[test]
fn a_listener_removed_before_the_cursor_reaches_it_is_skipped() {
    let mut tui = tui();
    let registry = tui.color_scheme_listener_registry();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    let later: Arc<Mutex<Option<ColorSchemeListenerId>>> = Arc::new(Mutex::new(None));
    let cell = Arc::clone(&later);
    let from_inside = registry.clone();
    let first_calls = Arc::clone(&calls);
    tui.add_color_scheme_listener(Arc::new(move |_scheme: TerminalColorScheme| {
        first_calls.lock().expect("lock").push("a");
        if let Some(id) = cell.lock().expect("lock").take() {
            from_inside.remove(id);
        }
    }));
    tui.add_color_scheme_listener(labelled(&calls, "b"));
    let c_calls = Arc::clone(&calls);
    let id = tui.add_color_scheme_listener(Arc::new(move |_scheme: TerminalColorScheme| {
        c_calls.lock().expect("lock").push("c");
    }));
    *later.lock().expect("lock") = Some(id);

    tui.consume_terminal_response(DARK);
    assert_eq!(
        log(&calls),
        vec!["a", "b"],
        "c went before the cursor reached it"
    );
}

/// The walk is live in the other direction too: an entry appended while it
/// runs lands in the set being iterated, so the walk visits it in the same
/// report. The listener is built inside the call and handed to the host
/// through a cell, because the registration needs `&mut Tui` the dispatch
/// holds — pi needs neither, since it edits the set directly.
#[test]
fn a_listener_registered_during_the_dispatch_is_visited() {
    let mut tui = tui();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    let built: Arc<Mutex<Option<ColorSchemeListener>>> = Arc::new(Mutex::new(None));
    let cell = Arc::clone(&built);
    let first_calls = Arc::clone(&calls);
    tui.add_color_scheme_listener(Arc::new(move |_scheme: TerminalColorScheme| {
        first_calls.lock().expect("lock").push("first");
        let sink = Arc::clone(&first_calls);
        *cell.lock().expect("lock") = Some(Arc::new(move |_scheme: TerminalColorScheme| {
            sink.lock().expect("lock").push("added");
        }));
    }));
    tui.add_color_scheme_listener(labelled(&calls, "second"));
    tui.consume_terminal_response(DARK);
    assert_eq!(
        log(&calls),
        vec!["first", "second"],
        "the new listener was not there yet"
    );

    // Registering it is a `Set.add` on the live set; the next report walks the
    // set as it stands, so the appended entry is visited in order.
    let added = built
        .lock()
        .expect("lock")
        .take()
        .expect("the first listener built one");
    tui.add_color_scheme_listener(added);
    calls.lock().expect("lock").clear();
    tui.consume_terminal_response(DARK);
    assert_eq!(log(&calls), vec!["first", "second", "added"]);
}

/// `tui.ts:943-948`: pi returns an unsubscribe closure; the id this port
/// returns removes exactly that listener and leaves the rest in registration
/// order. An id that already fired removes nothing, like `Set.delete`.
#[test]
fn removing_one_listener_leaves_the_others_registered_in_order() {
    let mut tui = tui();
    let registry = tui.color_scheme_listener_registry();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    let mut ids = Vec::new();
    for name in ["a", "b", "c"] {
        ids.push(tui.add_color_scheme_listener(labelled(&calls, name)));
    }
    assert_eq!(registry.len(), 3);

    tui.remove_color_scheme_listener(ids[1]);
    assert_eq!(registry.len(), 2);
    tui.consume_terminal_response(DARK);
    assert_eq!(log(&calls), vec!["a", "c"]);

    // Removing the same id again is pi's `Set.delete` of a missing reference.
    tui.remove_color_scheme_listener(ids[1]);
    assert_eq!(registry.len(), 2, "a second delete changes nothing");
    calls.lock().expect("lock").clear();
    tui.consume_terminal_response(LIGHT);
    assert_eq!(log(&calls), vec!["a", "c"]);
}

/// pi has no report queue (`tui.ts:1174-1176` notifies and returns); this
/// port's drain stays for hosts that poll instead of registering, and the
/// listeners still see every report.
#[test]
fn the_report_drain_and_the_listeners_both_see_the_reports() {
    let mut tui = tui();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    tui.add_color_scheme_listener(labelled(&calls, "listener"));

    tui.consume_terminal_response(LIGHT);
    tui.consume_terminal_response(DARK);
    assert_eq!(
        tui.take_color_scheme_reports(),
        vec![TerminalColorScheme::Light, TerminalColorScheme::Dark]
    );
    assert_eq!(log(&calls), vec!["listener", "listener"]);
    assert!(
        tui.take_color_scheme_reports().is_empty(),
        "the drain empties"
    );
}

/// pi's dispatch loop runs the listener with the parsed scheme
/// (`tui.ts:1174-1176`). A sequence that does not parse is not a report
/// (`tui.ts:1169-1171`), so it reaches no listener and is left for the input
/// path — the Rust port's `false` is pi's unconsumed input.
#[test]
fn an_unparsable_report_reaches_no_listener() {
    let mut tui = tui();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    tui.add_color_scheme_listener(labelled(&calls, "listener"));

    assert!(
        !tui.consume_terminal_response("\x1b[?997;3n"),
        "pi returns false, so input carries on"
    );
    assert_eq!(calls.lock().expect("lock").len(), 0);
    assert_eq!(
        tui.take_color_scheme_reports(),
        [] as [pi_tui::terminal_colors::TerminalColorScheme; 0]
    );
}

// ------------------------------------------------- the colour query itself

/// pi takes `onLateReply` as an **argument** of `queryTerminalColors`
/// (`tui.ts:1481-1487`), so it belongs to one query: with two in flight the
/// second has its own, and the deadline that fires for the first delivers
/// there and leaves the second resolving through the drain.
#[test]
fn a_late_reply_reaches_the_query_that_asked_for_it() {
    let mut tui = tui();
    let first_late: Arc<Mutex<Vec<RgbColor>>> = Arc::new(Mutex::new(Vec::new()));
    let second_late: Arc<Mutex<Vec<RgbColor>>> = Arc::new(Mutex::new(Vec::new()));

    let sink = Arc::clone(&first_late);
    tui.query_terminal_colors(
        100,
        Some(Box::new(move |colors| {
            if let Some(background) = colors.background {
                sink.lock().expect("lock").push(background);
            }
        })),
    )
    .expect("query");
    let sink = Arc::clone(&second_late);
    tui.query_terminal_colors(
        500,
        Some(Box::new(move |colors| {
            if let Some(background) = colors.background {
                sink.lock().expect("lock").push(background);
            }
        })),
    )
    .expect("query");

    // The first query's deadline passes; the second's has not.
    let settled = tui.service_terminal_queries(100);
    assert_eq!(settled.len(), 1, "only the first query's deadline fired");
    assert!(
        first_late.lock().expect("lock").is_empty(),
        "the deadline settles the promise, not the sink"
    );
    assert!(
        tui.take_terminal_colors().is_some(),
        "the deadline result joins the queue the host drains, like pi's resolve"
    );

    // The reply the first query was waiting for lands after its deadline.
    tui.consume_terminal_response("\x1b]11;#ffffff\x07");
    tui.consume_terminal_response(DA1);
    assert_eq!(
        first_late.lock().expect("lock").as_slice(),
        &[RgbColor {
            r: 255,
            g: 255,
            b: 255
        }],
        "the first query's own sink ran"
    );
    assert!(
        second_late.lock().expect("lock").is_empty(),
        "the second query was not touched"
    );

    // The second query is still live and still resolves through the drain.
    tui.consume_terminal_response("\x1b]11;#000000\x07");
    tui.consume_terminal_response(DA1);
    let colors = tui
        .take_terminal_colors()
        .expect("the second query completed through the drain");
    assert_eq!(colors.background, Some(RgbColor { r: 0, g: 0, b: 0 }));
    assert!(
        second_late.lock().expect("lock").is_empty(),
        "and never needed its sink"
    );
}

/// `tui.ts:1481-1484`: the deadline settles the query with the replies so far
/// and re-arms it for the late ones. A query that already completed is not
/// settled again — pi clears its timer in `completeTerminalColorQuery`.
#[test]
fn a_query_settles_once_across_its_deadline() {
    let mut tui = tui();
    let late: Arc<Mutex<Vec<TerminalColors>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&late);
    tui.query_terminal_colors(
        100,
        Some(Box::new(move |colors| {
            sink.lock().expect("lock").push(colors);
        })),
    )
    .expect("query");

    // The whole batch plus the fence lands before the deadline.
    tui.consume_terminal_response("\x1b]10;#ffffff\x07");
    tui.consume_terminal_response("\x1b]11;#000000\x07");
    for index in 0..TERMINAL_PALETTE_SIZE {
        tui.consume_terminal_response(&format!("\x1b]4;{index};#010203\x07"));
    }
    tui.consume_terminal_response(DA1);
    assert!(
        tui.take_terminal_colors().is_some(),
        "the promise resolved through the drain"
    );
    assert!(
        late.lock().expect("lock").is_empty(),
        "before the deadline the sink is not armed"
    );

    assert!(
        tui.service_terminal_queries(100).is_empty(),
        "pi cleared the timer when the query completed, so the deadline settles nothing"
    );
    assert!(
        tui.take_terminal_colors().is_none(),
        "and delivers nothing a second time"
    );
    assert!(late.lock().expect("lock").is_empty());

    // A reply that lands after the query settled is eaten but recorded nowhere
    // (`tui.ts:1132-1133`), so the sink stays untouched and nothing re-enters
    // the drain. The query is off the queue after its DA1 fence, so the reply
    // is ordinary input again.
    assert!(!tui.consume_terminal_response("\x1b]11;#00ff00\x07"));
    assert!(late.lock().expect("lock").is_empty());
    assert!(tui.take_terminal_colors().is_none());
}

/// The deadline is per query, not global: each pending query is expired by its
/// own `deadline_ms`, and one query's expiry never settles another's.
#[test]
fn each_query_expires_on_its_own_deadline() {
    let mut tui = tui();
    tui.query_terminal_colors(200, None).expect("query");
    tui.query_terminal_colors(400, None).expect("query");

    assert!(
        tui.service_terminal_queries(199).is_empty(),
        "neither has expired"
    );
    assert_eq!(
        tui.service_terminal_queries(200).len(),
        1,
        "the first one only"
    );
    assert_eq!(tui.service_terminal_queries(399).len(), 0, "and only once");
    assert_eq!(
        tui.service_terminal_queries(400).len(),
        1,
        "the second one at its own deadline"
    );

    // Two settled results, in the order their deadlines fired.
    assert_eq!(
        tui.take_terminal_colors().map(|colors| colors.background),
        Some(None)
    );
    assert_eq!(
        tui.take_terminal_colors().map(|colors| colors.background),
        Some(None)
    );
    assert!(tui.take_terminal_colors().is_none());
}

/// The replies still go to the oldest pending query only
/// (`tui.ts:1123`), so a terminal that answers in order fills the first query
/// and leaves the second empty until its own replies come.
#[test]
fn replies_go_to_the_oldest_pending_query() {
    let mut tui = tui();
    tui.query_terminal_colors(500, None).expect("query");
    tui.query_terminal_colors(500, None).expect("query");

    tui.consume_terminal_response("\x1b]11;#123456\x07");
    tui.consume_terminal_response(DA1);
    let first = tui.take_terminal_colors().expect("the first query");
    assert_eq!(
        first.background,
        Some(RgbColor {
            r: 0x12,
            g: 0x34,
            b: 0x56
        })
    );
    assert!(
        tui.take_terminal_colors().is_none(),
        "the DA1 shifted the first query off, not the second"
    );

    tui.consume_terminal_response("\x1b]11;#654321\x07");
    tui.consume_terminal_response(DA1);
    let second = tui.take_terminal_colors().expect("the second query");
    assert_eq!(
        second.background,
        Some(RgbColor {
            r: 0x65,
            g: 0x43,
            b: 0x21
        })
    );
}

/// With no query pending, a colour reply is ordinary input again
/// (`tui.ts:1123-1125`), which is what the pi test asserts.
#[test]
fn a_colour_reply_without_a_query_is_not_consumed() {
    let mut tui = tui();
    tui.query_terminal_colors(100, None).expect("query");
    tui.consume_terminal_response(DA1);
    assert!(!tui.consume_terminal_response("\x1b]11;#ffffff\x07"));
}
