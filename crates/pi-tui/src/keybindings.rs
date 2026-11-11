//! Key action names and pi's default key table.
//!
//! Ground truth: `packages/tui/src/keybindings.ts:72-158` (`TUI_KEYBINDINGS`)
//! and `:9-43` (`TUIKeyAction`). pi resolves an action through a configurable
//! binding map; the spike ships the defaults, which is what every component in
//! this crate matches against (the select list hard-codes the same ids).

use crate::keys::matches_key;

/// Every action pi's editor and select list can bind. `keybindings.ts:9-43`.
pub mod action {
    pub const EDITOR_CURSOR_UP: &str = "tui.editor.cursorUp";
    pub const EDITOR_CURSOR_DOWN: &str = "tui.editor.cursorDown";
    pub const EDITOR_HISTORY_PREVIOUS: &str = "tui.editor.historyPrevious";
    pub const EDITOR_HISTORY_NEXT: &str = "tui.editor.historyNext";
    pub const EDITOR_CURSOR_LEFT: &str = "tui.editor.cursorLeft";
    pub const EDITOR_CURSOR_RIGHT: &str = "tui.editor.cursorRight";
    pub const EDITOR_CURSOR_WORD_LEFT: &str = "tui.editor.cursorWordLeft";
    pub const EDITOR_CURSOR_WORD_RIGHT: &str = "tui.editor.cursorWordRight";
    pub const EDITOR_CURSOR_LINE_START: &str = "tui.editor.cursorLineStart";
    pub const EDITOR_CURSOR_LINE_END: &str = "tui.editor.cursorLineEnd";
    pub const EDITOR_JUMP_FORWARD: &str = "tui.editor.jumpForward";
    pub const EDITOR_JUMP_BACKWARD: &str = "tui.editor.jumpBackward";
    pub const EDITOR_PAGE_UP: &str = "tui.editor.pageUp";
    pub const EDITOR_PAGE_DOWN: &str = "tui.editor.pageDown";
    pub const EDITOR_DELETE_CHAR_BACKWARD: &str = "tui.editor.deleteCharBackward";
    pub const EDITOR_DELETE_CHAR_FORWARD: &str = "tui.editor.deleteCharForward";
    pub const EDITOR_DELETE_WORD_BACKWARD: &str = "tui.editor.deleteWordBackward";
    pub const EDITOR_DELETE_WORD_FORWARD: &str = "tui.editor.deleteWordForward";
    pub const EDITOR_DELETE_TO_LINE_START: &str = "tui.editor.deleteToLineStart";
    pub const EDITOR_DELETE_TO_LINE_END: &str = "tui.editor.deleteToLineEnd";
    pub const EDITOR_YANK: &str = "tui.editor.yank";
    pub const EDITOR_YANK_POP: &str = "tui.editor.yankPop";
    pub const EDITOR_UNDO: &str = "tui.editor.undo";
    pub const INPUT_NEW_LINE: &str = "tui.input.newLine";
    pub const INPUT_SUBMIT: &str = "tui.input.submit";
    pub const INPUT_TAB: &str = "tui.input.tab";
    pub const INPUT_COPY: &str = "tui.input.copy";
    pub const SELECT_UP: &str = "tui.select.up";
    pub const SELECT_DOWN: &str = "tui.select.down";
    pub const SELECT_PAGE_UP: &str = "tui.select.pageUp";
    pub const SELECT_PAGE_DOWN: &str = "tui.select.pageDown";
    pub const SELECT_CONFIRM: &str = "tui.select.confirm";
    pub const SELECT_CANCEL: &str = "tui.select.cancel";

    // Alternate-screen viewport navigation. pi notes that these "intentionally
    // shadow the unmodified editor bindings in fullscreen mode"
    // (`keybindings.ts:159`), so the same key reaches the viewport and not the
    // editor while the alt screen owns the tree.
    pub const ALT_SCREEN_PAGE_UP: &str = "tui.altScreen.pageUp";
    pub const ALT_SCREEN_PAGE_DOWN: &str = "tui.altScreen.pageDown";
    pub const ALT_SCREEN_HALF_PAGE_UP: &str = "tui.altScreen.halfPageUp";
    pub const ALT_SCREEN_HALF_PAGE_DOWN: &str = "tui.altScreen.halfPageDown";
    pub const ALT_SCREEN_LINE_UP: &str = "tui.altScreen.lineUp";
    pub const ALT_SCREEN_LINE_DOWN: &str = "tui.altScreen.lineDown";
    pub const ALT_SCREEN_PREVIOUS_PROMPT: &str = "tui.altScreen.previousPrompt";
    pub const ALT_SCREEN_NEXT_PROMPT: &str = "tui.altScreen.nextPrompt";
    pub const ALT_SCREEN_SEARCH: &str = "tui.altScreen.search";
    pub const ALT_SCREEN_SEARCH_NEXT: &str = "tui.altScreen.searchNext";
    pub const ALT_SCREEN_SEARCH_PREVIOUS: &str = "tui.altScreen.searchPrevious";
    pub const ALT_SCREEN_SEARCH_CLOSE: &str = "tui.altScreen.searchClose";
    pub const ALT_SCREEN_TOP: &str = "tui.altScreen.top";
    pub const ALT_SCREEN_BOTTOM: &str = "tui.altScreen.bottom";
}

/// pi's defaults: action id → key ids, in `keybindings.ts` order.
pub const DEFAULTS: &[(&str, &[&str])] = &[
    (action::EDITOR_CURSOR_UP, &["up"]),
    (action::EDITOR_CURSOR_DOWN, &["down"]),
    (action::EDITOR_HISTORY_PREVIOUS, &[]),
    (action::EDITOR_HISTORY_NEXT, &[]),
    (action::EDITOR_CURSOR_LEFT, &["left", "ctrl+b"]),
    (action::EDITOR_CURSOR_RIGHT, &["right", "ctrl+f"]),
    (
        action::EDITOR_CURSOR_WORD_LEFT,
        &["alt+left", "ctrl+left", "alt+b"],
    ),
    (
        action::EDITOR_CURSOR_WORD_RIGHT,
        &["alt+right", "ctrl+right", "alt+f"],
    ),
    (
        action::EDITOR_CURSOR_LINE_START,
        &["home", "ctrl+home", "ctrl+a"],
    ),
    (
        action::EDITOR_CURSOR_LINE_END,
        &["end", "ctrl+end", "ctrl+e"],
    ),
    (action::EDITOR_JUMP_FORWARD, &["ctrl+]"]),
    (action::EDITOR_JUMP_BACKWARD, &["ctrl+alt+]"]),
    (action::EDITOR_PAGE_UP, &["pageUp", "ctrl+pageUp"]),
    (action::EDITOR_PAGE_DOWN, &["pageDown", "ctrl+pageDown"]),
    (action::EDITOR_DELETE_CHAR_BACKWARD, &["backspace"]),
    (action::EDITOR_DELETE_CHAR_FORWARD, &["delete", "ctrl+d"]),
    (
        action::EDITOR_DELETE_WORD_BACKWARD,
        &["ctrl+w", "alt+backspace"],
    ),
    (action::EDITOR_DELETE_WORD_FORWARD, &["alt+d", "alt+delete"]),
    (action::EDITOR_DELETE_TO_LINE_START, &["ctrl+u"]),
    (action::EDITOR_DELETE_TO_LINE_END, &["ctrl+k"]),
    (action::EDITOR_YANK, &["ctrl+y"]),
    (action::EDITOR_YANK_POP, &["alt+y"]),
    (action::EDITOR_UNDO, &["ctrl+-"]),
    (action::INPUT_NEW_LINE, &["shift+enter", "ctrl+j"]),
    (action::INPUT_SUBMIT, &["enter"]),
    (action::INPUT_TAB, &["tab"]),
    (action::INPUT_COPY, &["ctrl+c"]),
    (action::SELECT_UP, &["up"]),
    (action::SELECT_DOWN, &["down"]),
    (action::SELECT_PAGE_UP, &["pageUp"]),
    (action::SELECT_PAGE_DOWN, &["pageDown"]),
    (action::SELECT_CONFIRM, &["enter"]),
    (action::SELECT_CANCEL, &["escape", "ctrl+c"]),
    (action::ALT_SCREEN_PAGE_UP, &["pageUp"]),
    (action::ALT_SCREEN_PAGE_DOWN, &["pageDown"]),
    // `keybindings.ts:170-181`: unbound in pi's defaults, kept unbound here.
    (action::ALT_SCREEN_HALF_PAGE_UP, &[]),
    (action::ALT_SCREEN_HALF_PAGE_DOWN, &[]),
    (action::ALT_SCREEN_LINE_UP, &[]),
    (action::ALT_SCREEN_LINE_DOWN, &[]),
    (
        action::ALT_SCREEN_PREVIOUS_PROMPT,
        &["ctrl+shift+up", "ctrl+up"],
    ),
    (
        action::ALT_SCREEN_NEXT_PROMPT,
        &["ctrl+shift+down", "ctrl+down"],
    ),
    (action::ALT_SCREEN_SEARCH, &["ctrl+shift+f"]),
    (action::ALT_SCREEN_SEARCH_NEXT, &["enter", "ctrl+g"]),
    (
        action::ALT_SCREEN_SEARCH_PREVIOUS,
        &["shift+enter", "ctrl+shift+g"],
    ),
    (action::ALT_SCREEN_SEARCH_CLOSE, &["escape"]),
    (action::ALT_SCREEN_TOP, &["home"]),
    (action::ALT_SCREEN_BOTTOM, &["end"]),
];

/// The keys bound to `action` in pi's defaults. `keybindings.ts:60-70`
/// (`getKeys`).
pub fn keys_for(action: &str) -> &'static [&'static str] {
    DEFAULTS
        .iter()
        .find(|(id, _)| *id == action)
        .map_or(&[], |(_, keys)| *keys)
}

/// Whether `data` matches `action`. pi's `kb.matches(data, action)`.
pub fn matches(data: &str, action: &str) -> bool {
    keys_for(action).iter().any(|key| matches_key(data, key))
}

/// The action a key id is bound to, for a host that wants the reverse lookup
/// (pi's `keybindings.ts` exposes it for its help output).
pub fn action_for(key: &str) -> Option<&'static str> {
    DEFAULTS
        .iter()
        .find(|(_, keys)| keys.contains(&key))
        .map(|(action, _)| *action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_defaults_match_pi() {
        assert!(matches("\x1b[A", action::EDITOR_CURSOR_UP));
        assert!(
            matches("\x02", action::EDITOR_CURSOR_LEFT),
            "ctrl+b is cursor left"
        );
        assert!(
            matches("\x01", action::EDITOR_CURSOR_LINE_START),
            "ctrl+a is line start"
        );
        assert!(matches("\x05", action::EDITOR_CURSOR_LINE_END));
        assert!(matches("\x17", action::EDITOR_DELETE_WORD_BACKWARD));
        assert!(matches("\x0b", action::EDITOR_DELETE_TO_LINE_END));
        assert!(matches("\x19", action::EDITOR_YANK));
        assert!(matches("\x1by", action::EDITOR_YANK_POP));
        assert!(matches("\x1f", action::EDITOR_UNDO));
        assert!(matches("\r", action::INPUT_SUBMIT));
        assert!(
            matches("\x0a", action::INPUT_NEW_LINE),
            "ctrl+j inserts a newline"
        );
        assert!(matches_key("\x1b[13;2u", "shift+enter"));
        assert!(matches("\t", action::INPUT_TAB));
        assert!(matches("\x03", action::INPUT_COPY));
    }

    #[test]
    fn history_actions_have_no_default_keys() {
        assert!(keys_for(action::EDITOR_HISTORY_PREVIOUS).is_empty());
        assert!(keys_for(action::EDITOR_HISTORY_NEXT).is_empty());
        assert!(!matches("\x1b[A", action::EDITOR_HISTORY_PREVIOUS));
    }

    #[test]
    fn shift_backspace_and_shift_delete_are_extra_matches() {
        // `editor.ts:800,805` matches `shift+backspace` / `shift+delete` beside
        // the bound keys.
        assert!(matches_key("\x1b[127;2u", "shift+backspace"));
        assert!(matches_key("\x1b[3;2~", "shift+delete"));
    }

    #[test]
    fn every_default_action_is_unique_per_key() {
        // pi allows one action per key across the editor's table; the select
        // list deliberately shadows up/down/enter, which is why those pairs
        // exist. Assert the ids resolve both ways.
        assert_eq!(
            action_for("ctrl+w"),
            Some(action::EDITOR_DELETE_WORD_BACKWARD)
        );
        assert_eq!(action_for("escape"), Some(action::SELECT_CANCEL));
        assert_eq!(action_for("nonexistent"), None);
    }

    /// `keybindings.ts:159-208`: the alt-screen ids, and the note that they
    /// shadow the editor's own pageUp/pageDown/home/end in fullscreen mode.
    #[test]
    fn alt_screen_defaults_match_pi() {
        assert!(matches("\x1b[5~", action::ALT_SCREEN_PAGE_UP));
        assert!(matches("\x1b[6~", action::ALT_SCREEN_PAGE_DOWN));
        assert!(
            matches("\x1b[1;5A", action::ALT_SCREEN_PREVIOUS_PROMPT),
            "ctrl+up"
        );
        assert!(
            matches("\x1b[1;5B", action::ALT_SCREEN_NEXT_PROMPT),
            "ctrl+down"
        );
        assert!(
            matches("\x1b[1;6A", action::ALT_SCREEN_PREVIOUS_PROMPT),
            "ctrl+shift+up"
        );
        assert!(
            matches_key("\x1b[102;6u", "ctrl+shift+f"),
            "the search id's only key"
        );
        assert!(
            matches("\r", action::ALT_SCREEN_SEARCH_NEXT),
            "enter is searchNext"
        );
        assert!(matches("\x07", action::ALT_SCREEN_SEARCH_NEXT), "ctrl+g");
        assert!(matches_key("\x1b[13;2u", "shift+enter"));
        assert!(
            matches_key("\x1b[103;6u", "ctrl+shift+g"),
            "searchPrevious's second key"
        );
        assert!(
            matches("\x1b", action::ALT_SCREEN_SEARCH_CLOSE),
            "escape closes search"
        );
        assert!(matches("\x1b[H", action::ALT_SCREEN_TOP));
        assert!(matches("\x1b[F", action::ALT_SCREEN_BOTTOM));
        // The viewport ids shadow the editor's, so a key resolves to the first
        // table entry (the editor) and the caller picks the alt-screen one.
        assert!(matches("\x1b[5~", action::EDITOR_PAGE_UP));
    }

    #[test]
    fn unbound_alt_screen_actions_stay_unbound() {
        // `keybindings.ts:170-181`: these four ship with no default keys.
        for action in [
            action::ALT_SCREEN_HALF_PAGE_UP,
            action::ALT_SCREEN_HALF_PAGE_DOWN,
            action::ALT_SCREEN_LINE_UP,
            action::ALT_SCREEN_LINE_DOWN,
        ] {
            assert!(
                keys_for(action).is_empty(),
                "{action} must have no default keys"
            );
        }
    }

    #[test]
    fn select_page_actions_match_pi() {
        // `keybindings.ts:151-155`.
        assert!(matches("\x1b[5~", action::SELECT_PAGE_UP));
        assert!(matches("\x1b[6~", action::SELECT_PAGE_DOWN));
    }
}
