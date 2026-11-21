# `/mcp` manager UI — coordinator wiring notes

Component landed: `crates/pi-coding-agent/src/modes/interactive/components/mcp_selector.rs`
(branch `feat/mcp-ui`). It is intentionally self-contained: it knows nothing
about the MCP manager, config files, or OAuth. It renders the four upstream
screens and publishes typed events; the coordinator supplies data and acts on
the events.

## 0. Module registration (required, not done in this worktree)

Per the slice boundary I did **not** edit shared files. Add to
`crates/pi-coding-agent/src/modes/interactive/components/mod.rs` (alphabetical,
after `markdown_transform`):

```rust
pub mod mcp_selector;
```

Without this one line the component is not compiled and its tests do not run.

## 1. `theme.rs`

**No changes required.** The component uses only existing actions:
`tui.select.confirm`, `tui.select.cancel` (`pi-tui` registry) and
`app.message.copy` (`theme::app_keys::MESSAGE_COPY`). It deliberately does not
call `theme::key_hint` for `tui.*` actions: that helper only resolves the app
registry and would render an empty key. Its local `key_text`/`key_hint`
resolve both registries and join all bound keys with `/`, matching upstream's
`keyHint` (`enter manage • escape/ctrl+c close`).

## 2. `interactive_mode.rs` imports and state

Next to the other component imports (~line 68–86):

```rust
use super::components::mcp_selector::{
    McpMenu, McpSelectorComponent, McpSelectorEvent, McpSelectorScreen,
};
```

Next to the other selector event fields (~line 1624–1676):

```rust
mcp_selector_events: Option<Arc<Mutex<Vec<McpSelectorEvent>>>>,
```

Initialize it to `None` wherever the sibling fields are initialized (the
`InteractiveMode` constructor / test builders — search for
`session_selector_events: None`).

## 3. Command dispatch and autocomplete

In `InteractiveMode::submit` (the `/trust`/`/settings`/`/resume` block around
lines 3708–3722), add:

```rust
// The MCP manager UI (`extensions/mcp/index.ts:1261-1287`).
if text == "/mcp" {
    self.open_mcp_selector();
    return;
}
```

Upstream `/mcp` also accepts `login <server>`, `logout <server>` and
`reconnect <server>` (plain status when there is no TUI). Those arms need the
MCP manager data source and are out of this slice; route any `/mcp ...` text
to the existing non-TUI/plain-status path until that source lands.

In `autocomplete_commands` (~line 2581, next to `trust`/`settings`/`resume`),
add the description verbatim from `mcp/index.ts:1262`:

```rust
autocomplete::SlashCommand::new(
    "mcp",
    "Manage MCP servers: sign in, reconnect, enable or disable, and change exposure",
),
```

## 4. Mount / unmount (mirrors `open_session_selector`, lines 6676–6701)

```rust
fn open_mcp_selector(&mut self) {
    if self.suspended_editor.is_some() {
        return;
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    // Starts on the constructor's `Loading…` screen.
    let selector = McpSelectorComponent::new(Arc::clone(&events));
    self.mcp_selector_events = Some(events);
    self.suspend_editor_with(Box::new(selector));
    // Then immediately push the current manager snapshot:
    //   self.set_mcp_menu(build_servers_menu(...));
}

fn close_mcp_selector(&mut self) {
    if self.suspended_editor.is_none() {
        return;
    }
    self.mcp_selector_events = None;
    self.restore_editor();
}
```

`suspend_editor_with` (line 6883) already does the required
`Tui::replace_child` swap through `replace_editor_slot` (line 6865): flat mode
uses `self.tui.replace_child(HostSlot::Editor.index(), component)`; fullscreen
uses the viewport dock's `VStack::replace_child(HostSlot::Editor.dock_index(),
component)`. It parks the displaced editor via `park_displaced_occupant` and
sets focus by component id. The component implements `as_any_mut`, so
`downcast_tree::<McpSelectorComponent>(HostSlot::Editor)` works for the
rebuild calls below (a missing `as_any_mut` would silently no-op).

To deliver data to the mounted component without a second borrow of `self`,
use the established downcast pattern (see the trust-selector test at
`interactive_mode.rs:15435`):

```rust
if let Some(view) = self.downcast_tree::<McpSelectorComponent>(HostSlot::Editor) {
    view.set_menu(menu);                 // subscribe rebuild, keeps cursor
    // or: view.set_screen(McpSelectorScreen::Status { .. });
    // or: view.set_screen(McpSelectorScreen::RedirectUrl { .. });
}
```

### API semantics

| Method | Upstream equivalent | Effect |
|---|---|---|
| `McpSelectorComponent::new(events)` | constructor (`ui.ts:84-89`) | `MCP servers` / `Loading…` frame |
| `set_screen(McpSelectorScreen::Menu(m))` | a **new** `menu()` call | resets preserved value to `m.selected` |
| `set_menu(m)` | `subscribe` listener's `render()` | rebuilds from `m`, keeps the last highlighted value (`wanted = selected ?? m.selected`) |
| `set_screen(Status { title, message, cancellable })` | `status()` | `cancellable = onCancel.is_some()`; adds `escape/ctrl+c cancel` footer |
| `set_screen(RedirectUrl { title, authorization_url })` | `redirectUrl()` | fresh input, focus restored, OSC-8 link + hint |
| `selected_value()` | closure-local `selected` | the value a rebuild would keep |
| `drain()` | — | take queued events (the mode may instead read the `Arc` directly) |

## 5. Event drain in the pump

Add `self.pump_mcp_selector();` to the pump's selector-drain block (currently
lines 11545–11549: model, scoped-models, trust, settings, session), and a
handler next to `pump_trust_selector` (lines 6622–6656). Take the queue out
first so the mutex guard does not pin `self` across handlers:

```rust
fn pump_mcp_selector(&mut self) {
    let Some(events) = self.mcp_selector_events.as_ref() else { return; };
    let events = std::mem::take(&mut *events.lock().expect("mcp selector events"));
    for event in events {
        match event {
            // The chosen item's `value`: "enable", "signin", "tools", a server
            // name from the top menu, an exposure id, or a tool name. Dispatch
            // through the manager's `runAction`/server-menu state machine.
            McpSelectorEvent::Select(value) => { /* ... */ }
            McpSelectorEvent::SelectionChanged(_) => {} // already kept internally
            McpSelectorEvent::Cancel => self.close_mcp_selector(),
            // The status screen stays up: the callback aborts the sign-in.
            McpSelectorEvent::StatusCancelled => { /* abort OAuth */ }
            McpSelectorEvent::RedirectUrl(url) => { /* complete OAuth with url */ }
            McpSelectorEvent::CopyUrl(url) => { /* clipboard write, then re-render */ }
        }
    }
}
```

Select events follow upstream's nested loop (`mcp/index.ts:1001-1012`): the
top menu resolves a **server name**, then the server menu resolves an
**action** (`enable`, `enable-project`, `disable`, `disable-project`, `signin`,
`reconnect`, `signout`, `tools`, `exposure`); `back`/`close` are cancels. After
an action, re-emit the manager's change notification and call `set_menu` on
the mounted view so the subscribed menu rebuilds.

## 6. Data strings to feed the component

These are the exact upstream strings (`mcp/index.ts:791-943`); the component
renders whatever it is given, so build `McpMenu`/`Status` with these:

- Top menu: title `MCP servers`, `error` = `config: …` / `overridden: …` lines,
  `empty` = `No MCP servers configured. Add them to <agentDir>/mcp.json or .pi/mcp.json.`,
  `confirm_label` = `manage`, `cancel_label` = `close`.
- Server menu: title `MCP server <name>`, multiline `details` (transport,
  `<scope>: <source>`, optional `project override: <path>`, `State: …`),
  `confirm_label` = `select`, `cancel_label` = `back`. Missing server →
  title `<name>`, empty, `This server is no longer configured.`, labels `""`/`back`.
- Tools: title `Tools of <name>`, `Exposure <exposure>: <description>`, empty
  `The server offers no tools.`, labels `back`/`back`.
- Exposure: title `Exposure of <name>`, labels `save`/`back`, `selected` = current.
- Sign-in status: title `Sign in to <name>`; messages
  `Contacting the authorization server…`, `Connecting…`, cancellable.
- Redirect screen footer: `enter submit • escape/ctrl+c cancel`.

## 7. Deviations / known gaps

- `McpManagerView.capturesViewportInput` (`ui.ts:100-102`) has no counterpart
  in the port's `Component` trait; the mode's existing focus/suspend routing
  is used instead.
- Mouse forwarding to the inner `SelectList` is not wired (upstream's
  `SelectList` handles clicks/wheel). Keys are fully ported.
- `app.message.copy` is emitted as `CopyUrl(String)`; the coordinator owns
  clipboard I/O. There is no `ctx.ui.notify` success/error feedback in the
  component.
- No search field: upstream `ui.ts` never calls `SelectList.setFilter`, so
  there is nothing to port here (filtering, if wanted, belongs to the data
  builder).
- The URL hint uses `Ctrl+click to open` unconditionally (POSIX-only port;
  upstream switches to `Cmd+click` on darwin).
