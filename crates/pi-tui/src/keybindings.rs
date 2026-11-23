//! Key action names, pi's default key table, and the configurable manager.
//!
//! Ground truth: `packages/tui/src/keybindings.ts:72-158` (`TUI_KEYBINDINGS`),
//! `:9-43` (`TUIKeyAction`), `:206-315` (`KeybindingsManager`,
//! `getKeybindings`/`setKeybindings`, conflict detection) and
//! `packages/coding-agent/src/core/keybindings.ts:201-357` (legacy-name
//! migration, `<agentDir>/keybindings.json` loading, `reload`).
//!
//! Every component in the port matches through [`matches`]/[`keys_for`], just
//! as upstream's components match through the global manager's `matches`/
//! `getKeys`. Installing a [`KeybindingsManager`] with [`install`] makes those
//! functions resolve user overrides; with no manager installed they fall back
//! to [`DEFAULTS`], exactly like upstream's lazily created default manager.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use crate::keys::{SYMBOL_KEYS, matches_key};

/// Every action pi's editor and select list can bind. `keybindings.ts:9-43`.
pub mod action {
    /// Move the editor cursor up one line.
    pub const EDITOR_CURSOR_UP: &str = "tui.editor.cursorUp";
    /// Move the editor cursor down one line.
    pub const EDITOR_CURSOR_DOWN: &str = "tui.editor.cursorDown";
    /// Step back through the input history.
    ///
    /// Unbound in pi's default table; a host supplies its own history keys.
    pub const EDITOR_HISTORY_PREVIOUS: &str = "tui.editor.historyPrevious";
    /// Step forward through the input history.
    ///
    /// Unbound in pi's default table; a host supplies its own history keys.
    pub const EDITOR_HISTORY_NEXT: &str = "tui.editor.historyNext";
    /// Move the editor cursor one column left.
    pub const EDITOR_CURSOR_LEFT: &str = "tui.editor.cursorLeft";
    /// Move the editor cursor one column right.
    pub const EDITOR_CURSOR_RIGHT: &str = "tui.editor.cursorRight";
    /// Move the editor cursor to the start of the word before it.
    pub const EDITOR_CURSOR_WORD_LEFT: &str = "tui.editor.cursorWordLeft";
    /// Move the editor cursor to the end of the word after it.
    pub const EDITOR_CURSOR_WORD_RIGHT: &str = "tui.editor.cursorWordRight";
    /// Move the editor cursor to the start of the line.
    pub const EDITOR_CURSOR_LINE_START: &str = "tui.editor.cursorLineStart";
    /// Move the editor cursor to the end of the line.
    pub const EDITOR_CURSOR_LINE_END: &str = "tui.editor.cursorLineEnd";
    /// Enter character-jump mode, moving forward to the next typed character.
    pub const EDITOR_JUMP_FORWARD: &str = "tui.editor.jumpForward";
    /// Enter character-jump mode, moving backward to the next typed character.
    pub const EDITOR_JUMP_BACKWARD: &str = "tui.editor.jumpBackward";
    /// Scroll the editor view up one page.
    pub const EDITOR_PAGE_UP: &str = "tui.editor.pageUp";
    /// Scroll the editor view down one page.
    pub const EDITOR_PAGE_DOWN: &str = "tui.editor.pageDown";
    /// Delete the character before the cursor.
    pub const EDITOR_DELETE_CHAR_BACKWARD: &str = "tui.editor.deleteCharBackward";
    /// Delete the character after the cursor.
    pub const EDITOR_DELETE_CHAR_FORWARD: &str = "tui.editor.deleteCharForward";
    /// Delete the word before the cursor.
    pub const EDITOR_DELETE_WORD_BACKWARD: &str = "tui.editor.deleteWordBackward";
    /// Delete the word after the cursor.
    pub const EDITOR_DELETE_WORD_FORWARD: &str = "tui.editor.deleteWordForward";
    /// Delete from the cursor back to the start of the line.
    pub const EDITOR_DELETE_TO_LINE_START: &str = "tui.editor.deleteToLineStart";
    /// Delete from the cursor to the end of the line.
    pub const EDITOR_DELETE_TO_LINE_END: &str = "tui.editor.deleteToLineEnd";
    /// Yank: paste the current kill-ring entry at the cursor.
    pub const EDITOR_YANK: &str = "tui.editor.yank";
    /// After a yank, cycle to the previous kill-ring entry.
    pub const EDITOR_YANK_POP: &str = "tui.editor.yankPop";
    /// Undo the last editor change.
    pub const EDITOR_UNDO: &str = "tui.editor.undo";
    /// Insert a newline into the input instead of submitting.
    pub const INPUT_NEW_LINE: &str = "tui.input.newLine";
    /// Submit the current input.
    pub const INPUT_SUBMIT: &str = "tui.input.submit";
    /// Trigger completion, or accept the open autocomplete's selection.
    pub const INPUT_TAB: &str = "tui.input.tab";
    /// Copy (or interrupt) request, left for the host to act on.
    pub const INPUT_COPY: &str = "tui.input.copy";
    /// Select list: move the highlight up one entry.
    pub const SELECT_UP: &str = "tui.select.up";
    /// Select list: move the highlight down one entry.
    pub const SELECT_DOWN: &str = "tui.select.down";
    /// Select list: page the highlight up.
    pub const SELECT_PAGE_UP: &str = "tui.select.pageUp";
    /// Select list: page the highlight down.
    pub const SELECT_PAGE_DOWN: &str = "tui.select.pageDown";
    /// Select list: confirm the highlighted entry.
    pub const SELECT_CONFIRM: &str = "tui.select.confirm";
    /// Select list: cancel without choosing.
    pub const SELECT_CANCEL: &str = "tui.select.cancel";

    // Alternate-screen viewport navigation. pi notes that these "intentionally
    // shadow the unmodified editor bindings in fullscreen mode"
    // (`keybindings.ts:159`), so the same key reaches the viewport and not the
    // editor while the alt screen owns the tree.
    /// Alternate screen: scroll the transcript up one page.
    pub const ALT_SCREEN_PAGE_UP: &str = "tui.altScreen.pageUp";
    /// Alternate screen: scroll the transcript down one page.
    pub const ALT_SCREEN_PAGE_DOWN: &str = "tui.altScreen.pageDown";
    /// Alternate screen: scroll up half a page.
    ///
    /// Unbound in pi's defaults (`keybindings.ts:170-181`).
    pub const ALT_SCREEN_HALF_PAGE_UP: &str = "tui.altScreen.halfPageUp";
    /// Alternate screen: scroll down half a page.
    ///
    /// Unbound in pi's defaults (`keybindings.ts:170-181`).
    pub const ALT_SCREEN_HALF_PAGE_DOWN: &str = "tui.altScreen.halfPageDown";
    /// Alternate screen: scroll up one line.
    ///
    /// Unbound in pi's defaults (`keybindings.ts:170-181`).
    pub const ALT_SCREEN_LINE_UP: &str = "tui.altScreen.lineUp";
    /// Alternate screen: scroll down one line.
    ///
    /// Unbound in pi's defaults (`keybindings.ts:170-181`).
    pub const ALT_SCREEN_LINE_DOWN: &str = "tui.altScreen.lineDown";
    /// Alternate screen: jump to the previous prompt.
    pub const ALT_SCREEN_PREVIOUS_PROMPT: &str = "tui.altScreen.previousPrompt";
    /// Alternate screen: jump to the next prompt.
    pub const ALT_SCREEN_NEXT_PROMPT: &str = "tui.altScreen.nextPrompt";
    /// Alternate screen: open the transcript search overlay.
    pub const ALT_SCREEN_SEARCH: &str = "tui.altScreen.search";
    /// Search overlay: move to the next match.
    pub const ALT_SCREEN_SEARCH_NEXT: &str = "tui.altScreen.searchNext";
    /// Search overlay: move to the previous match.
    pub const ALT_SCREEN_SEARCH_PREVIOUS: &str = "tui.altScreen.searchPrevious";
    /// Search overlay: close it.
    pub const ALT_SCREEN_SEARCH_CLOSE: &str = "tui.altScreen.searchClose";
    /// Alternate screen: jump to the top of the transcript.
    pub const ALT_SCREEN_TOP: &str = "tui.altScreen.top";
    /// Alternate screen: jump to the bottom of the transcript.
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
    // `keybindings.ts:83-88`: plain `home`/`end` move the editor cursor;
    // Ctrl+Home/Ctrl+End belong to the alt-screen viewport (`:216-222`).
    (action::EDITOR_CURSOR_LINE_START, &["home", "ctrl+a"]),
    (action::EDITOR_CURSOR_LINE_END, &["end", "ctrl+e"]),
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
    (action::ALT_SCREEN_TOP, &["ctrl+home"]),
    (action::ALT_SCREEN_BOTTOM, &["ctrl+end"]),
];

/// The keys bound to `action`: the installed manager's resolved bindings when
/// one is installed, pi's defaults otherwise. `keybindings.ts:60-70`
/// (`getKeys`), `:294-300` (`getKeybindings`).
pub fn keys_for(action: &str) -> &'static [&'static str] {
    global_keys_for(action).unwrap_or_else(|| default_keys_for(action))
}

/// One atomic read of the installed table: `Some(keys)` even when the action
/// is deliberately unbound, `None` when no manager defines it. Hosts with
/// their own action tables use this to avoid a check-then-read race when
/// managers are swapped between calls.
pub fn global_keys_for(action: &str) -> Option<&'static [&'static str]> {
    global_resolved().and_then(|shared| shared.read().expect("keybindings").keys(action))
}

fn default_keys_for(action: &str) -> &'static [&'static str] {
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
/// (pi's `keybindings.ts` exposes it for its help output). Resolved bindings
/// win over the defaults, in table order.
pub fn action_for(key: &str) -> Option<&'static str> {
    if let Some(shared) = global_resolved()
        && let Some(action) = shared.read().expect("keybindings").action_for(key)
    {
        return Some(action);
    }
    DEFAULTS
        .iter()
        .find(|(_, keys)| keys.contains(&key))
        .map(|(action, _)| *action)
}

/// Whether a manager is installed and defines `action` (even with no keys).
/// Hosts with their own action table use this to avoid mistaking a deliberately
/// disabled action for an unknown one.
pub fn is_globally_bound(action: &str) -> bool {
    global_resolved()
        .is_some_and(|shared| shared.read().expect("keybindings").keys(action).is_some())
}

// The manager's resolved table is shared with `keys_for`/`matches` through an
// `Arc`, so a `reload()` updates every consumer without rebinding components.
// Upstream's `setKeybindings` holds the same process-global reference
// (`keybindings.ts:291-300`). A thread-local layer is offered for tests so a
// scoped override cannot race a parallel test in the same binary.
static GLOBAL_MANAGER: OnceLock<RwLock<Option<Arc<RwLock<ResolvedBindings>>>>> = OnceLock::new();
thread_local! {
    static THREAD_MANAGER: RefCell<Option<Arc<RwLock<ResolvedBindings>>>> = const { RefCell::new(None) };
}

fn global_resolved() -> Option<Arc<RwLock<ResolvedBindings>>> {
    if let Some(shared) = THREAD_MANAGER.with(|cell| cell.borrow().clone()) {
        return Some(shared);
    }
    let slot = GLOBAL_MANAGER.get_or_init(|| RwLock::new(None));
    slot.read()
        .expect("keybindings global")
        .as_ref()
        .map(Arc::clone)
}

/// Key ids are interned so the `&'static [&'static str]` lookup signature can
/// stay stable across reloads without re-leaking unchanged keys or key lists.
fn intern(value: &str) -> &'static str {
    static INTERNED: OnceLock<RwLock<HashMap<String, &'static str>>> = OnceLock::new();
    let mut map = INTERNED
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .expect("keybindings interner");
    if let Some(existing) = map.get(value) {
        return existing;
    }
    let leaked: &'static str = Box::leak(value.to_string().into_boxed_str());
    map.insert(value.to_string(), leaked);
    leaked
}

fn intern_keys(keys: &[&'static str]) -> &'static [&'static str] {
    static LISTS: OnceLock<RwLock<HashMap<String, &'static [&'static str]>>> = OnceLock::new();
    let signature = keys.join("\0");
    let mut map = LISTS
        .get_or_init(|| RwLock::new(HashMap::new()))
        .write()
        .expect("keybindings interner");
    if let Some(existing) = map.get(&signature) {
        return existing;
    }
    let leaked: &'static [&'static str] = Box::leak(keys.to_vec().into_boxed_slice());
    map.insert(signature, leaked);
    leaked
}

/// Legacy config names from before the `tui.`/`app.` prefixing
/// (`core/keybindings.ts:201-261`, `KEYBINDING_NAME_MIGRATIONS`).
const LEGACY_NAMES: &[(&str, &str)] = &[
    ("cursorUp", "tui.editor.cursorUp"),
    ("cursorDown", "tui.editor.cursorDown"),
    ("cursorLeft", "tui.editor.cursorLeft"),
    ("cursorRight", "tui.editor.cursorRight"),
    ("cursorWordLeft", "tui.editor.cursorWordLeft"),
    ("cursorWordRight", "tui.editor.cursorWordRight"),
    ("cursorLineStart", "tui.editor.cursorLineStart"),
    ("cursorLineEnd", "tui.editor.cursorLineEnd"),
    ("jumpForward", "tui.editor.jumpForward"),
    ("jumpBackward", "tui.editor.jumpBackward"),
    ("pageUp", "tui.editor.pageUp"),
    ("pageDown", "tui.editor.pageDown"),
    ("deleteCharBackward", "tui.editor.deleteCharBackward"),
    ("deleteCharForward", "tui.editor.deleteCharForward"),
    ("deleteWordBackward", "tui.editor.deleteWordBackward"),
    ("deleteWordForward", "tui.editor.deleteWordForward"),
    ("deleteToLineStart", "tui.editor.deleteToLineStart"),
    ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
    ("yank", "tui.editor.yank"),
    ("yankPop", "tui.editor.yankPop"),
    ("undo", "tui.editor.undo"),
    ("newLine", "tui.input.newLine"),
    ("submit", "tui.input.submit"),
    ("tab", "tui.input.tab"),
    ("copy", "tui.input.copy"),
    ("selectUp", "tui.select.up"),
    ("selectDown", "tui.select.down"),
    ("selectPageUp", "tui.select.pageUp"),
    ("selectPageDown", "tui.select.pageDown"),
    ("selectConfirm", "tui.select.confirm"),
    ("selectCancel", "tui.select.cancel"),
    ("interrupt", "app.interrupt"),
    ("clear", "app.clear"),
    ("exit", "app.exit"),
    ("suspend", "app.suspend"),
    ("cycleThinkingLevel", "app.thinking.cycle"),
    ("cycleModelForward", "app.model.cycleForward"),
    ("cycleModelBackward", "app.model.cycleBackward"),
    ("selectModel", "app.model.select"),
    ("expandTools", "app.tools.expand"),
    ("toggleThinking", "app.thinking.toggle"),
    ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
    ("externalEditor", "app.editor.external"),
    ("followUp", "app.message.followUp"),
    ("dequeue", "app.message.dequeue"),
    ("pasteImage", "app.clipboard.pasteImage"),
    ("newSession", "app.session.new"),
    ("tree", "app.session.tree"),
    ("fork", "app.session.fork"),
    ("resume", "app.session.resume"),
    ("treeFoldOrUp", "app.tree.foldOrUp"),
    ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
    ("treeEditLabel", "app.tree.editLabel"),
    ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
    ("toggleSessionPath", "app.session.togglePath"),
    ("toggleSessionSort", "app.session.toggleSort"),
    ("renameSession", "app.session.rename"),
    ("deleteSession", "app.session.delete"),
    ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
];

fn migrated_name(id: &str) -> &str {
    LEGACY_NAMES
        .iter()
        .find(|(legacy, _)| *legacy == id)
        .map_or(id, |(_, name)| *name)
}

/// The base keys accepted by `KeyIdSchema` (`core/key-id-schema.ts:5-11`):
/// named keys from `packages/tui/src/keys.ts:165-196` plus single characters.
const NAMED_KEYS: &[&str] = &[
    "escape",
    "esc",
    "enter",
    "return",
    "tab",
    "space",
    "backspace",
    "delete",
    "insert",
    "clear",
    "home",
    "end",
    "pageUp",
    "pageDown",
    "up",
    "down",
    "left",
    "right",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
    "backtick",
    "hyphen",
    "equals",
    "leftbracket",
    "rightbracket",
    "backslash",
    "semicolon",
    "quote",
    "comma",
    "period",
    "slash",
    "exclamation",
    "at",
    "hash",
    "dollar",
    "percent",
    "caret",
    "ampersand",
    "asterisk",
    "leftparen",
    "rightparen",
    "underscore",
    "plus",
    "pipe",
    "tilde",
    "leftbrace",
    "rightbrace",
    "colon",
    "lessthan",
    "greaterthan",
    "question",
];

/// Whether `id` is a valid key id under upstream's `KeyIdSchema`: up to four
/// distinct modifiers (`ctrl`, `shift`, `alt`, `super`) and one base key.
pub fn is_valid_key_id(id: &str) -> bool {
    const MODIFIERS: &[&str] = &["ctrl", "shift", "alt", "super"];
    if id.is_empty() {
        return false;
    }
    let mut base = id;
    let mut seen = std::collections::HashSet::new();
    // Strip left-to-right so a symbol base like `+` (`ctrl++`) survives.
    while let Some((modifier, rest)) = MODIFIERS.iter().find_map(|modifier| {
        base.strip_prefix(*modifier)
            .and_then(|rest| rest.strip_prefix('+'))
            .map(|rest| (*modifier, rest))
    }) {
        if !seen.insert(modifier) {
            return false;
        }
        base = rest;
    }
    if NAMED_KEYS.contains(&base) {
        return true;
    }
    let mut chars = base.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() || SYMBOL_KEYS.contains(&c))
        && chars.next().is_none()
}

/// User overrides by id, in load order: `{ "id": "key" | ["key", ...] }`.
/// Invalid values are dropped like upstream's
/// `toKeybindingsConfig` (`core/keybindings.ts:267-276`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindingsConfig {
    entries: Vec<(String, Vec<String>)>,
}

impl KeybindingsConfig {
    /// An empty override set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a `keybindings.json` document, including BOM stripping, legacy
    /// name migration, `$schema` skipping, and key-id validation.
    pub fn from_json_str(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let Ok(serde_json::Value::Object(object)) = serde_json::from_str::<serde_json::Value>(text)
        else {
            return Self::default();
        };
        let raw: Vec<(String, serde_json::Value)> = object.into_iter().collect();
        // Upstream skips a legacy name when the migrated name is also present
        // (`migrateKeybindingsConfig`, `core/keybindings.ts:278-298`).
        let raw_names: std::collections::HashSet<String> =
            raw.iter().map(|(id, _)| id.clone()).collect();
        let mut migrated: Vec<(String, serde_json::Value)> = Vec::new();
        for (id, value) in raw {
            if id == "$schema" {
                continue;
            }
            let name = migrated_name(&id);
            if name != id && raw_names.contains(name) {
                continue;
            }
            if let Some(slot) = migrated.iter_mut().find(|(existing, _)| existing == name) {
                slot.1 = value;
            } else {
                migrated.push((name.to_string(), value));
            }
        }
        let mut config = Self::new();
        for (id, value) in migrated {
            let keys = match value {
                serde_json::Value::String(key) => {
                    if is_valid_key_id(&key) {
                        vec![key]
                    } else {
                        continue;
                    }
                }
                serde_json::Value::Array(items) => {
                    let mut keys = Vec::with_capacity(items.len());
                    let mut valid = true;
                    for item in items {
                        let serde_json::Value::String(key) = item else {
                            valid = false;
                            break;
                        };
                        if !is_valid_key_id(&key) {
                            valid = false;
                            break;
                        }
                        keys.push(key);
                    }
                    if !valid {
                        continue;
                    }
                    keys
                }
                _ => continue,
            };
            config.insert(id, keys);
        }
        config
    }

    /// Set (or replace) the keys for `id`; an empty list disables the action.
    pub fn insert(&mut self, id: impl Into<String>, keys: impl Into<Vec<String>>) {
        let id = id.into();
        let keys = keys.into();
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|(existing, _)| *existing == id)
        {
            slot.1 = keys;
        } else {
            self.entries.push((id, keys));
        }
    }

    /// The keys for `id`, if overridden.
    pub fn get(&self, id: &str) -> Option<&[String]> {
        self.entries
            .iter()
            .find(|(existing, _)| existing == id)
            .map(|(_, keys)| keys.as_slice())
    }

    /// Iterate over `(id, keys)` in load order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.entries
            .iter()
            .map(|(id, keys)| (id.as_str(), keys.as_slice()))
    }

    /// Number of overridden ids.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no overrides.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Two or more actions claiming the same key (`KeybindingConflict` upstream,
/// `packages/tui/src/keybindings.ts:209-212,242-250`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    /// The contested key id.
    pub key: String,
    /// The actions claiming it, in config order.
    pub keybindings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct ResolvedBindings {
    entries: Vec<(&'static str, &'static [&'static str])>,
    index: HashMap<&'static str, usize>,
}

impl ResolvedBindings {
    fn keys(&self, action: &str) -> Option<&'static [&'static str]> {
        self.index
            .get(action)
            .map(|&position| self.entries[position].1)
    }

    fn action_for(&self, key: &str) -> Option<&'static str> {
        self.entries
            .iter()
            .find(|(_, keys)| keys.contains(&key))
            .map(|(action, _)| *action)
    }
}

fn normalized_keys(keys: &[String]) -> Vec<&'static str> {
    let mut seen = std::collections::HashSet::new();
    keys.iter()
        .filter(|key| seen.insert(key.as_str()))
        .map(|key| intern(key))
        .collect()
}

/// Resolve defaults with the user's overrides and compute conflicts. Mirrors
/// upstream's `rebuild` (`packages/tui/src/keybindings.ts:227-253`).
fn rebuild(
    definitions: &[(String, Vec<String>)],
    user: &KeybindingsConfig,
) -> (ResolvedBindings, Vec<KeybindingConflict>) {
    let known: std::collections::HashSet<&str> =
        definitions.iter().map(|(id, _)| id.as_str()).collect();
    // User claims are collected in config order; each action claims a key once.
    let mut claims: Vec<(&str, Vec<&str>)> = Vec::new();
    for (id, keys) in user.iter() {
        if !known.contains(id) {
            continue;
        }
        for key in normalized_keys(keys) {
            if let Some((_, keys)) = claims.iter_mut().find(|(claimant, _)| *claimant == id) {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            } else {
                claims.push((id, vec![key]));
            }
        }
    }
    let mut conflicts: Vec<KeybindingConflict> = Vec::new();
    for (key, claimants) in {
        let mut by_key: Vec<(&str, Vec<&str>)> = Vec::new();
        for (id, keys) in &claims {
            for key in keys {
                if let Some((_, claimants)) = by_key.iter_mut().find(|(claimed, _)| claimed == key)
                {
                    claimants.push(id);
                } else {
                    by_key.push((key, vec![id]));
                }
            }
        }
        by_key
    } {
        if claimants.len() > 1 {
            conflicts.push(KeybindingConflict {
                key: key.to_string(),
                keybindings: claimants.into_iter().map(str::to_string).collect(),
            });
        }
    }

    let mut entries: Vec<(&'static str, &'static [&'static str])> = definitions
        .iter()
        .map(|(id, defaults)| {
            let keys = user.get(id).unwrap_or(defaults.as_slice());
            (intern(id), intern_keys(&normalized_keys(keys)))
        })
        .collect();
    // Extra ids are kept sorted after the known ones, mirroring upstream's
    // `orderKeybindingsConfig` (`core/keybindings.ts:300-316`).
    let mut extras: Vec<(&str, &[String])> =
        user.iter().filter(|(id, _)| !known.contains(id)).collect();
    extras.sort_by_key(|(left, _)| *left);
    entries.extend(
        extras
            .into_iter()
            .map(|(id, keys)| (intern(id), intern_keys(&normalized_keys(keys)))),
    );
    let index = entries
        .iter()
        .enumerate()
        .map(|(position, (id, _))| (*id, position))
        .collect();
    (ResolvedBindings { entries, index }, conflicts)
}

/// A keybinding manager: definitions (default keys per action) plus the user's
/// `<agentDir>/keybindings.json` overrides. Port of upstream's TUI
/// `KeybindingsManager` and the coding agent's file-loading subclass
/// (`packages/tui/src/keybindings.ts:221-290`,
/// `packages/coding-agent/src/core/keybindings.ts:329-357`).
#[derive(Debug, Clone)]
pub struct KeybindingsManager {
    definitions: Arc<Vec<(String, Vec<String>)>>,
    user_bindings: KeybindingsConfig,
    config_path: Option<PathBuf>,
    resolved: Arc<RwLock<ResolvedBindings>>,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    /// A manager over the given definitions with no config file (the TUI's
    /// `new KeybindingsManager(TUI_KEYBINDINGS)`).
    pub fn new(definitions: &[(&str, &[&str])]) -> Self {
        Self::with_definitions(definitions, None)
    }

    /// A manager over the given definitions, loading `config_path` when set.
    /// Hosts pass the full TUI + app table and `<agentDir>/keybindings.json`.
    pub fn with_definitions(definitions: &[(&str, &[&str])], config_path: Option<PathBuf>) -> Self {
        let definitions: Vec<(String, Vec<String>)> = definitions
            .iter()
            .map(|(id, keys)| {
                (
                    (*id).to_string(),
                    keys.iter().map(|key| (*key).to_string()).collect(),
                )
            })
            .collect();
        let user_bindings = config_path
            .as_ref()
            .and_then(|path| Self::load_from_file(path))
            .unwrap_or_default();
        let (resolved, conflicts) = rebuild(&definitions, &user_bindings);
        Self {
            definitions: Arc::new(definitions),
            user_bindings,
            config_path,
            resolved: Arc::new(RwLock::new(resolved)),
            conflicts,
        }
    }

    /// Upstream's `KeybindingsManager.create(agentDir)`:
    /// `<agentDir>/keybindings.json` over the TUI defaults. Interactive mode
    /// uses [`Self::with_definitions`] to add the app-level actions too.
    pub fn create(agent_dir: impl AsRef<Path>) -> Self {
        Self::with_definitions(DEFAULTS, Some(agent_dir.as_ref().join("keybindings.json")))
    }

    fn load_from_file(path: &Path) -> Option<KeybindingsConfig> {
        let text = std::fs::read_to_string(path).ok()?;
        Some(KeybindingsConfig::from_json_str(&text))
    }

    /// Publish this manager to every `matches`/`keys_for` caller (upstream's
    /// `setKeybindings`, `packages/tui/src/keybindings.ts:293-295`).
    pub fn install(&self) {
        GLOBAL_MANAGER
            .get_or_init(|| RwLock::new(None))
            .write()
            .expect("keybindings global")
            .replace(Arc::clone(&self.resolved));
    }

    /// Install only on the current thread; the returned guard restores the
    /// previous layer on drop. Test seam for scoped override assertions.
    #[doc(hidden)]
    pub fn install_on_current_thread(&self) -> ThreadBindingsGuard {
        let previous = THREAD_MANAGER.with(|cell| {
            let previous = cell.borrow().clone();
            *cell.borrow_mut() = Some(Arc::clone(&self.resolved));
            previous
        });
        ThreadBindingsGuard { previous }
    }

    /// Re-read the config file and update every installed consumer. Returns
    /// the fresh conflicts (upstream's `reload`,
    /// `core/keybindings.ts:343-346`).
    pub fn reload(&mut self) -> Vec<KeybindingConflict> {
        if let Some(path) = &self.config_path {
            self.user_bindings = Self::load_from_file(path).unwrap_or_default();
            self.rebuild_in_place();
        }
        self.conflicts.clone()
    }

    /// Replace the overrides directly (upstream's `setUserBindings`).
    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.rebuild_in_place();
    }

    fn rebuild_in_place(&mut self) {
        let (resolved, conflicts) = rebuild(&self.definitions, &self.user_bindings);
        *self.resolved.write().expect("keybindings") = resolved;
        self.conflicts = conflicts;
    }

    /// The keys currently bound to `action` (upstream's `getKeys`).
    pub fn keys_for(&self, action: &str) -> Vec<&'static str> {
        self.resolved
            .read()
            .expect("keybindings")
            .keys(action)
            .map_or_else(Vec::new, <[&'static str]>::to_vec)
    }

    /// Whether raw input matches `action` under this manager.
    pub fn matches(&self, data: &str, action: &str) -> bool {
        self.keys_for(action)
            .iter()
            .any(|key| matches_key(data, key))
    }

    /// All key conflicts in the user's config (upstream's `getConflicts`).
    pub fn conflicts(&self) -> &[KeybindingConflict] {
        &self.conflicts
    }

    /// The user's overrides (upstream's `getUserBindings`).
    pub fn user_bindings(&self) -> &KeybindingsConfig {
        &self.user_bindings
    }

    /// Every action and its resolved keys, in table order: the shape
    /// extension shortcut resolution consumes (upstream's
    /// `getResolvedBindings`, `packages/tui/src/keybindings.ts:307-312`).
    pub fn get_effective_config(&self) -> Vec<(&'static str, &'static [&'static str])> {
        self.resolved.read().expect("keybindings").entries.clone()
    }
}

/// Restores the previous thread-local keybinding layer on drop.
#[doc(hidden)]
pub struct ThreadBindingsGuard {
    previous: Option<Arc<RwLock<ResolvedBindings>>>,
}

impl Drop for ThreadBindingsGuard {
    fn drop(&mut self) {
        THREAD_MANAGER.with(|cell| *cell.borrow_mut() = self.previous.take());
    }
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
        // `keybindings.ts:83-88,216-222`: plain home/end move the editor
        // cursor; the viewport's top/bottom use the ctrl-modified variants.
        assert!(matches("\x1b[H", action::EDITOR_CURSOR_LINE_START));
        assert!(!matches("\x1b[H", action::ALT_SCREEN_TOP));
        assert!(matches("\x1b[7^", action::ALT_SCREEN_TOP));
        assert!(!matches("\x1b[7^", action::EDITOR_CURSOR_LINE_START));
        assert!(matches("\x1b[F", action::EDITOR_CURSOR_LINE_END));
        assert!(!matches("\x1b[F", action::ALT_SCREEN_BOTTOM));
        assert!(matches("\x1b[8^", action::ALT_SCREEN_BOTTOM));
        assert!(!matches("\x1b[8^", action::EDITOR_CURSOR_LINE_END));
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

    #[test]
    fn config_parsing_validates_migrates_and_disables() {
        // `core/keybindings.ts:267-298`, `key-id-schema.ts:5-21`.
        let config = KeybindingsConfig::from_json_str(
            "\u{feff}{\"$schema\": \"x\", \"submit\": \"ctrl+shift+enter\", \
             \"yank\": [\"ctrl+y\", \"ctrl+alt+y\"], \"undo\": [], \
             \"bogus\": 12, \"badKey\": \"ctrl+ctrl+x\", \"halfBad\": [\"ctrl+x\", 3]}",
        );
        assert_eq!(
            config.get("tui.input.submit"),
            Some(&["ctrl+shift+enter".to_string()][..])
        );
        assert_eq!(
            config.get("tui.editor.yank"),
            Some(&["ctrl+y".to_string(), "ctrl+alt+y".to_string()][..])
        );
        assert_eq!(
            config.get("tui.editor.undo"),
            Some(&[][..]),
            "empty disables"
        );
        assert!(config.get("$schema").is_none());
        assert!(config.get("bogus").is_none(), "invalid values are dropped");
        assert!(
            config.get("badKey").is_none(),
            "duplicate modifiers are invalid"
        );
        assert!(
            config.get("halfBad").is_none(),
            "one invalid key drops the array"
        );
        assert!(!is_valid_key_id("ctrl+ctrl+x"));
        assert!(is_valid_key_id("ctrl+-"));
        assert!(is_valid_key_id("ctrl++"), "a symbol base may be `+`");
        assert!(is_valid_key_id("pageUp"));
        assert!(!is_valid_key_id("CTRL+x"), "base keys are lowercase");
        assert!(!is_valid_key_id(""));
    }

    #[test]
    fn config_reports_user_conflicts() {
        // `packages/tui/src/keybindings.ts:240-250`: two ids claiming one key.
        let mut user = KeybindingsConfig::new();
        user.insert(action::EDITOR_YANK, vec!["ctrl+x".to_string()]);
        user.insert(action::INPUT_COPY, vec!["ctrl+x".to_string()]);
        let mut manager = KeybindingsManager::new(&[
            (action::EDITOR_YANK, &["ctrl+y"][..]),
            (action::INPUT_COPY, &["ctrl+c"][..]),
        ]);
        manager.set_user_bindings(user);
        assert_eq!(
            manager.conflicts(),
            &[KeybindingConflict {
                key: "ctrl+x".to_string(),
                keybindings: vec![
                    action::EDITOR_YANK.to_string(),
                    action::INPUT_COPY.to_string()
                ]
            }]
        );
    }

    #[test]
    fn override_changes_dispatched_key_and_restores_after_guard() {
        // The global `matches` used by every editor/selector component must
        // see the manager's resolved keys (`keybindings.ts:293-300`).
        let mut user = KeybindingsConfig::new();
        user.insert(action::INPUT_SUBMIT, vec!["ctrl+shift+enter".to_string()]);
        let mut manager = KeybindingsManager::new(DEFAULTS);
        manager.set_user_bindings(user);
        {
            let _guard = manager.install_on_current_thread();
            assert!(matches("\x1b[13;6u", action::INPUT_SUBMIT));
            assert!(!matches("\r", action::INPUT_SUBMIT));
            assert_eq!(keys_for(action::INPUT_SUBMIT), &["ctrl+shift+enter"]);
            assert_eq!(action_for("ctrl+shift+enter"), Some(action::INPUT_SUBMIT));
        }
        assert!(
            matches("\r", action::INPUT_SUBMIT),
            "guard restores defaults"
        );
    }

    #[test]
    fn reload_picks_up_file_changes() {
        // `core/keybindings.ts:337-346`: create/read/reload from agentDir.
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let agent_dir = std::env::temp_dir().join(format!(
            "pi-rs-keybindings-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        let config_path = agent_dir.join("keybindings.json");
        std::fs::write(
            &config_path,
            format!("{{\"{}\": \"ctrl+shift+enter\"}}", action::INPUT_SUBMIT),
        )
        .expect("write keybindings");
        let mut manager = KeybindingsManager::create(&agent_dir);
        assert!(manager.matches("\x1b[13;6u", action::INPUT_SUBMIT));
        std::fs::write(
            &config_path,
            format!("{{\"{}\": \"ctrl+shift+j\"}}", action::INPUT_SUBMIT),
        )
        .expect("rewrite keybindings");
        assert!(
            manager.matches("\x1b[13;6u", action::INPUT_SUBMIT),
            "until reload the old keys stay live"
        );
        assert!(manager.reload().is_empty());
        assert!(manager.matches("\x1b[106;6u", action::INPUT_SUBMIT));
        assert!(!manager.matches("\x1b[13;6u", action::INPUT_SUBMIT));
        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&agent_dir);
    }
}
