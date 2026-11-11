//! Port of `packages/tui/test/mouse-components.test.ts`.
//!
//! Cases that need the alt-screen renderer (`TuiAltScreen`) live in
//! `alt_screen_mouse.rs`; the dispatch state machine itself is pinned there
//! too. Everything here is pure component behaviour, matching the
//! component-level half of pi's test file.

use pi_tui::ansi::strip_terminal_sequences;
use pi_tui::component::Component;
use pi_tui::components::editor::{Editor, EditorOptions, EditorTheme};
use pi_tui::components::input::Input;
use pi_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use pi_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListAction, SettingsListTheme,
};
use pi_tui::mouse::{MouseButton, MouseEvent, MouseEventKind};

/// `mouse()`, `mouse-components.test.ts:12-30`.
fn mouse(kind: MouseEventKind, x: i64, y: i64, width: usize, height: usize) -> MouseEvent {
    let mut event = MouseEvent::new(kind, 0, x, y, width, height);
    if kind == MouseEventKind::Click {
        event = event.with_click_count(1);
    }
    event
}

fn move_mouse(kind: MouseEventKind, x: i64, y: i64, width: usize, height: usize) -> MouseEvent {
    // `button: "none"` for hover (`mouse-components.test.ts:161`).
    MouseEvent::new(kind, 3, x, y, width, height)
}

fn wheel(x: i64, y: i64, delta: i64) -> MouseEvent {
    MouseEvent::new(MouseEventKind::Wheel, 0, x, y, 80, 10).with_wheel_delta(delta)
}

fn select_list(count: usize, max_visible: usize) -> SelectList {
    let items = (0..count)
        .map(|index| SelectItem::new(format!("item-{index}"), format!("Item {index}")))
        .collect();
    SelectList::new(
        items,
        max_visible,
        SelectListTheme::default(),
        SelectListLayout::default(),
    )
}

/// pi's `settingsTheme` (`mouse-components.test.ts:49-55`) — notably a `"> "`
/// cursor, not the Rust default arrow.
fn settings_theme() -> SettingsListTheme {
    SettingsListTheme {
        cursor: "> ".to_string(),
        ..SettingsListTheme::default()
    }
}

fn settings_list(count: usize, max_visible: usize) -> SettingsList {
    let items = (0..count)
        .map(|index| {
            SettingItem::new(format!("item-{index}"), format!("Item {index}"), "off")
                .with_description(format!("Description {index}"))
                .with_values(vec!["off".to_string(), "on".to_string()])
        })
        .collect();
    SettingsList::new(items, max_visible, settings_theme())
}

/// "positions a single-line input cursor on press". `mouse-components.test.ts:80-86`.
#[test]
fn positions_a_single_line_input_cursor_on_press() {
    let mut input = Input::with_prompt("> ");
    input.set_value("hello");
    input.handle_key("\x1b[F");
    input.render(20);

    assert_eq!(
        input
            .handle_mouse(&mouse(MouseEventKind::Press, 4, 0, 20, 1))
            .unwrap()
            .handled,
        Some(true)
    );
    input.handle_key("X");
    assert_eq!(input.value(), "heXllo");
}

/// "selects and activates list rows". `mouse-components.test.ts:88-107`.
#[test]
fn selects_and_activates_list_rows() {
    let items = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|v| SelectItem::new(*v, v.to_uppercase()))
        .collect();
    let mut list = SelectList::new(
        items,
        3,
        SelectListTheme::default(),
        SelectListLayout::default(),
    );

    assert_eq!(
        list.handle_mouse(&mouse(MouseEventKind::Press, 1, 2, 40, 3))
            .unwrap()
            .handled,
        Some(true)
    );
    assert_eq!(
        list.selected_item().map(|item| item.value.as_str()),
        Some("c")
    );
    assert_eq!(
        list.handle_mouse(&mouse(MouseEventKind::Click, 1, 2, 40, 3))
            .unwrap()
            .handled,
        Some(true)
    );
    assert_eq!(
        list.selected_item().map(|item| item.value.as_str()),
        Some("c")
    );
    assert_eq!(
        list.take_action(),
        pi_tui::components::select_list::SelectListAction::Confirmed
    );
}

/// "activates settings rows". `mouse-components.test.ts:109-126`.
#[test]
fn activates_settings_rows() {
    let items = vec![
        SettingItem::new("mode", "Mode", "one").with_values(vec!["one".into(), "two".into()]),
        SettingItem::new("other", "Other", "off").with_values(vec!["off".into(), "on".into()]),
        SettingItem::new("third", "Third", "low").with_values(vec!["low".into(), "high".into()]),
        SettingItem::new("fourth", "Fourth", "x").with_values(vec!["x".into(), "y".into()]),
    ];
    let mut list = SettingsList::new(items, 3, settings_theme());

    list.handle_mouse(&mouse(MouseEventKind::Press, 1, 2, 40, 5));
    list.handle_mouse(&mouse(MouseEventKind::Click, 1, 2, 40, 5));
    assert_eq!(
        list.take_action(),
        SettingsListAction::Changed {
            id: "third".into(),
            value: "high".into()
        }
    );
}

/// The loop over `row` at `mouse-components.test.ts:128`. pi runs it for rows
/// 0 and 4; both are pinned here.
const ROWS: [i64; 2] = [0, 4];

/// "ignores hover and clicks visible select-list row N after scrolling".
/// `mouse-components.test.ts:128-165`.
#[test]
fn ignores_hover_and_clicks_visible_select_list_rows_after_scrolling() {
    for row in ROWS {
        let mut list = select_list(12, 5);
        list.set_selected_index(5);

        let result = list.handle_mouse(&wheel(1, row, 1)).expect("wheel handled");
        assert_eq!(result.handled, Some(true));
        assert_eq!(result.render, Some(true));
        assert_eq!(
            list.selected_item().map(|item| item.value.as_str()),
            Some("item-6")
        );
        assert_eq!(list.selection_change_count(), 1, "row {row}");

        let before = list.render(80);
        assert!(
            strip_terminal_sequences(&before[row as usize])
                .trim_end()
                .ends_with(&format!("Item {}", 4 + row)),
            "row {row}: {:?}",
            strip_terminal_sequences(&before[row as usize])
        );

        for y in [0, 1, 2, 3, 4, row] {
            assert_eq!(
                list.handle_mouse(&move_mouse(MouseEventKind::Move, 1, y, 80, 10)),
                None,
                "hover at y={y} row {row} must be unhandled"
            );
            assert_eq!(list.render(80), before, "hover must not repaint");
        }
        assert_eq!(
            list.selected_item().map(|item| item.value.as_str()),
            Some("item-6")
        );
        assert_eq!(
            list.selection_change_count(),
            1,
            "hover fired a change notification"
        );
        assert_eq!(
            list.take_action(),
            pi_tui::components::select_list::SelectListAction::None
        );

        list.handle_mouse(&mouse(MouseEventKind::Press, 1, row, 80, 10));
        list.render(80);
        list.handle_mouse(&mouse(MouseEventKind::Click, 1, row, 80, 10));
        assert_eq!(
            list.selected_item().map(|item| item.value.as_str()),
            Some(format!("item-{}", 4 + row).as_str()),
            "row {row}"
        );
        assert_eq!(list.selection_change_count(), 2, "row {row}");
        assert_eq!(
            list.take_action(),
            pi_tui::components::select_list::SelectListAction::Confirmed
        );
    }
}

/// "ignores hover and clicks visible settings row N after scrolling".
/// `mouse-components.test.ts:167-198`.
///
/// pi runs this with `{enableSearch: true}`, which shifts every mouse row and
/// every rendered row by two (`settings-list.ts:185-190`). This port renders
/// no search box — it has had none since 4b — so both are unshifted: the
/// selected row is body row 2 rather than absolute row 4, and item rows sit
/// at `row` rather than `row + 2`. The wheel, hover and press/click semantics
/// are identical either way.
#[test]
fn ignores_hover_and_clicks_visible_settings_rows_after_scrolling() {
    for row in ROWS {
        let mut list = settings_list(12, 5);
        list.select_item("item-5");

        list.handle_mouse(&wheel(1, row, 1));
        assert_eq!(
            list.selected_item().map(|item| item.id.as_str()),
            Some("item-6"),
            "row {row}"
        );
        let before = list.render(80);
        assert!(
            strip_terminal_sequences(&before[2]).starts_with("> Item 6"),
            "row {row}: {:?}",
            strip_terminal_sequences(&before[2])
        );
        assert!(
            strip_terminal_sequences(&before[row as usize]).contains(&format!("Item {} ", 4 + row)),
            "row {row}: {:?}",
            strip_terminal_sequences(&before[row as usize])
        );

        for y in [0, 1, 2, 3, 4, row] {
            assert_eq!(
                list.handle_mouse(&move_mouse(MouseEventKind::Move, 1, y, 80, 10)),
                None,
                "hover at y={y} row {row} must be unhandled"
            );
            assert_eq!(list.render(80), before, "hover must not repaint");
        }
        assert_eq!(list.take_action(), SettingsListAction::None);

        list.handle_mouse(&mouse(MouseEventKind::Press, 1, row, 80, 10));
        list.render(80);
        list.handle_mouse(&mouse(MouseEventKind::Click, 1, row, 80, 10));
        assert_eq!(
            list.take_action(),
            SettingsListAction::Changed {
                id: format!("item-{}", 4 + row),
                value: "on".into()
            },
            "row {row}"
        );
    }
}

/// `MouseButton` round-trips through the raw button code, the way the SGR
/// decode feeds it (`tui-alt-screen.ts:786-799`).
#[test]
fn wheel_events_carry_no_button() {
    let event = wheel(1, 2, -3);
    assert_eq!(event.button, MouseButton::None);
    assert_eq!(event.wheel_delta, Some(-3));
    let right_click = MouseEvent::new(MouseEventKind::Press, 2, 1, 2, 80, 10);
    assert_eq!(right_click.button, MouseButton::Right);
}

/// An editor sized and rendered once, so `last_width` and the visible line
/// count match a real frame.
fn rendered_editor(text: &str, width: usize, padding_x: usize) -> Editor {
    let options = EditorOptions {
        padding_x,
        ..EditorOptions::default()
    };
    let mut editor = Editor::new(EditorTheme::default(), options);
    editor.insert_text_at_cursor(text);
    editor.render_lines(width, 24);
    editor
}

/// Visible column of the `index`-th grapheme, which is what a click carries.
/// `Editor::handleMouse` walks `visibleWidth` per grapheme (`editor.ts:672`).
fn column_of_grapheme(text: &str, index: usize) -> i64 {
    use unicode_segmentation::UnicodeSegmentation;
    let mut column = 0i64;
    for (position, grapheme) in text.graphemes(true).enumerate() {
        if position == index {
            return column;
        }
        column += pi_tui::ansi::visible_width(grapheme) as i64;
    }
    column
}

/// F3 regression: `grapheme.index` is a byte offset and the cursor counts
/// chars (`editor.ts:679`, `:690`), so a click right of any multi-byte
/// character used to land further right than the clicked grapheme.
///
/// Row 0 is the editor's top border, so the body starts at y = 1. Each case
/// pins the cursor to the char index of the grapheme that was clicked.
#[test]
fn a_click_lands_on_the_clicked_grapheme_after_multibyte_text() {
    for (text, clicked_grapheme) in [
        ("héllo", 2),
        ("héllo", 3),
        ("世界 hello", 3),
        ("ok 🙂 tail", 3),
        ("plain ascii", 4),
    ] {
        let width = 40;
        let padding_x = 1;
        let mut editor = rendered_editor(text, width, padding_x);

        let x = padding_x as i64 + column_of_grapheme(text, clicked_grapheme);
        let event = mouse(MouseEventKind::Click, x, 1, width, 3);
        let result = editor.handle_mouse(&event).expect("click handled");
        assert_eq!(result.focus, Some(true));
        assert_eq!(
            editor.get_cursor(),
            (0, clicked_grapheme),
            "click on {text:?} grapheme {clicked_grapheme}"
        );
    }
}

/// Press, drag and release are declined so the renderer's screen-level text
/// selection can run over the editor rows (`editor.ts:655-659`).
#[test]
fn editor_leaves_non_click_gestures_unhandled() {
    let mut editor = rendered_editor("hello", 40, 0);
    let resting = editor.get_cursor();
    for kind in [
        MouseEventKind::Press,
        MouseEventKind::Drag,
        MouseEventKind::Release,
    ] {
        let event = mouse(kind, 2, 1, 40, 3);
        assert_eq!(
            editor.handle_mouse(&event),
            None,
            "{kind:?} must fall through"
        );
    }
    assert_eq!(editor.get_cursor(), resting);
}

/// A click in the editor border rows or past the body is still claimed and
/// focused, just without moving the cursor (`editor.ts:660`, `:665`).
#[test]
fn editor_claims_clicks_outside_the_body_without_moving_the_cursor() {
    for y in [0, 2, 3, 9] {
        let mut editor = rendered_editor("hello", 40, 0);
        let resting = editor.get_cursor();
        let event = mouse(MouseEventKind::Click, 2, y, 40, 3);
        let result = editor.handle_mouse(&event).expect("claimed");
        assert_eq!(result.handled, Some(true), "row {y}");
        assert_eq!(result.focus, Some(true), "row {y}");
        assert_eq!(
            editor.get_cursor(),
            resting,
            "row {y} must not move the cursor"
        );
    }
}
