//! `McpManagerView` — the `/mcp` manager UI (component layer).
//!
//! Ground truth: `packages/coding-agent/src/extensions/mcp/ui.ts:26-268`
//! (`McpMenu`, `McpUi`, `McpManagerView`, `showMcpManager`) and the exact
//! menu/status strings its `manage` callbacks build in
//! `packages/coding-agent/src/extensions/mcp/index.ts:791-943`. Framing and
//! the 12-row cap come from `ui.ts:58-71`; the link/copy screen mirrors
//! `modes/interactive/components/auth-url.ts:1-39`.
//!
//! Kept self-contained: data arrives through [`McpSelectorScreen`] and
//! [`McpMenu`], and every upstream callback (`onSelect`,
//! `onSelectionChange`, `onCancel`, `status`'s `onCancel`, the redirect-URL
//! submit and copy) becomes a [`McpSelectorEvent`] in the queue the mode
//! drains in its pump. Upstream's `subscribe` rebuild maps to
//! [`McpSelectorComponent::set_menu`], which rebuilds the list while keeping
//! the highlighted value.

use std::sync::{Arc, Mutex};

use pi_tui::ansi::{truncate_to_width, visible_width, wrap_text_with_ansi};
use pi_tui::component::{Component, ComponentMutation, Focusable};
use pi_tui::components::input::{Input, InputOptions};
use pi_tui::components::select_list::{SelectItem, SelectList, SelectListAction, SelectListLayout};
use pi_tui::keybindings;
use pi_tui::keybindings::action;
use pi_tui::terminal_image::hyperlink;
use pi_tui::theme::ThemeColor;

use crate::modes::interactive::theme;

/// The largest number of rows a menu's `SelectList` shows (`ui.ts:71`, `:143`).
pub const MAX_VISIBLE_ITEMS: usize = 12;

/// One menu, as rebuilt by `ui.ts:113-162`'s `build()`. `SelectItem` is the
/// same shape upstream imports from `@earendil-works/pi-tui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpMenu {
    pub title: String,
    /// Shown below the title (`ui.ts:28-29`).
    pub details: Option<String>,
    /// Shown below the details, in the error color (`ui.ts:30-31`).
    pub error: Option<String>,
    pub items: Vec<SelectItem>,
    /// Shown when there are no items (`ui.ts:33-34`).
    pub empty: Option<String>,
    /// Value selected when the menu opens (`ui.ts:35-36`).
    pub selected: Option<String>,
    /// What the confirm key does, for the key hint (`ui.ts:37-38`).
    pub confirm_label: String,
    /// What the cancel key does, for the key hint (`ui.ts:39-40`).
    pub cancel_label: String,
}

impl McpMenu {
    /// A menu with no details/error/preselect. The caller fills the labels.
    pub fn new(title: impl Into<String>, items: Vec<SelectItem>) -> Self {
        Self {
            title: title.into(),
            details: None,
            error: None,
            items,
            empty: None,
            selected: None,
            confirm_label: String::new(),
            cancel_label: String::new(),
        }
    }

    /// Builder: the line shown when the menu has no items.
    #[must_use]
    pub fn with_empty(mut self, empty: impl Into<String>) -> Self {
        self.empty = Some(empty.into());
        self
    }

    /// Builder: the value highlighted when the menu opens.
    #[must_use]
    pub fn with_selected(mut self, selected: impl Into<String>) -> Self {
        self.selected = Some(selected.into());
        self
    }
}

/// What the manager view currently shows. Each variant is one `setContent`
/// call in `ui.ts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSelectorScreen {
    /// The constructor's placeholder (`ui.ts:88`).
    Loading,
    /// A `menu()` view (`ui.ts:113-162`).
    Menu(McpMenu),
    /// A `status()` view (`ui.ts:164-175`). `cancellable` is whether an
    /// `onCancel` callback was supplied: it adds the cancel footer and makes
    /// the cancel key publish [`McpSelectorEvent::StatusCancelled`].
    Status {
        title: String,
        message: String,
        cancellable: bool,
    },
    /// The `redirectUrl()` sign-in view (`ui.ts:177-234`).
    RedirectUrl {
        title: String,
        authorization_url: String,
    },
}

/// Everything the view publishes. `Select`/`Cancel` are the menu's
/// `finish` outcomes; `SelectionChanged` feeds `selected` across `subscribe`
/// rebuilds; the rest are the sign-in screens' callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSelectorEvent {
    /// `onSelect` — the highlighted menu item was confirmed.
    Select(String),
    /// `onSelectionChange` — the cursor moved to this item's value.
    SelectionChanged(String),
    /// `onCancel` — cancel on a menu or the redirect-URL screen.
    Cancel,
    /// The status screen's `onCancel` callback fired (the screen stays up).
    StatusCancelled,
    /// A non-empty pasted redirect URL was submitted.
    RedirectUrl(String),
    /// `app.message.copy` on the sign-in screen: copy this authorization URL.
    CopyUrl(String),
}

/// `formatKeyText` + `keyHint` (`keybinding-hints.ts:29-44`): every bound key
/// joined with `/`, then dim key + muted description. Resolving both
/// registries here (instead of `theme::key_hint`, which only knows app
/// actions) is what makes `tui.select.*` hints show `enter`/`escape/ctrl+c`
/// like upstream.
fn key_text(action: &str) -> String {
    let tui_keys = pi_tui::keybindings::keys_for(action);
    let keys = if tui_keys.is_empty() {
        theme::app_keys::keys_for(action)
    } else {
        tui_keys
    };
    keys.join("/")
}

fn key_hint(action: &str, description: &str) -> String {
    let palette = theme::theme();
    format!(
        "{} {}",
        palette.fg(ThemeColor::Dim, &key_text(action)),
        palette.fg(ThemeColor::Muted, description)
    )
}

/// The lines of `Text(content, 1, 0)` (`text.ts:66-124`): wrap at
/// `width - 2`, one cell of margin on each side, then pad to the viewport.
fn text_lines(content: &str, width: usize) -> Vec<String> {
    if content.trim().is_empty() {
        return Vec::new();
    }
    let padding_x = 1.min(width.saturating_sub(1) / 2);
    let content_width = width.saturating_sub(padding_x * 2).max(1);
    let normalized = content.replace('\t', "   ");
    let margin = " ".repeat(padding_x);
    wrap_text_with_ansi(&normalized, content_width)
        .into_iter()
        .map(|line| {
            let with_margins = format!("{margin}{line}{margin}");
            let padding = " ".repeat(width.saturating_sub(visible_width(&with_margins)));
            format!("{with_margins}{padding}")
        })
        .collect()
}

/// Pad a line rendered by a child component (the list or the input), which
/// already owns its own prefixes. `frame()` concatenates children verbatim.
fn pad_line(line: &str, width: usize) -> String {
    let clipped = truncate_to_width(line, width, "", false);
    let padding = " ".repeat(width.saturating_sub(visible_width(&clipped)));
    format!("{clipped}{padding}")
}

/// A `Spacer(1)` line.
fn blank_line(width: usize) -> String {
    " ".repeat(width)
}

/// `frame()`'s accent border (`ui.ts:60`, `:67`).
fn border_line(width: usize) -> String {
    theme::theme().fg(ThemeColor::Accent, &"─".repeat(width))
}

/// `buildSelectList` inside `menu()` (`ui.ts:141-148`): the window is capped
/// at twelve rows and the wanted value is restored, if it still exists.
fn build_list(menu: &McpMenu, wanted: Option<&str>) -> Option<SelectList> {
    if menu.items.is_empty() {
        return None;
    }
    let max_visible = menu.items.len().min(MAX_VISIBLE_ITEMS);
    let mut list = SelectList::new(
        menu.items.clone(),
        max_visible,
        theme::select_list_theme(),
        SelectListLayout::default(),
    );
    if let Some(index) =
        wanted.and_then(|value| menu.items.iter().position(|item| item.value == value))
    {
        list.set_selected_index(index);
    }
    Some(list)
}

/// The manager view (`McpManagerView`, `ui.ts:75-250`).
pub struct McpSelectorComponent {
    screen: McpSelectorScreen,
    /// Rebuilt by `set_menu` for non-empty menus; `None` for the empty-menu,
    /// loading, status and redirect screens.
    list: Option<SelectList>,
    /// A fresh input for each redirect screen (`ui.ts:192`).
    input: Input,
    /// The last highlighted value — upstream's closure-local `selected`
    /// (`ui.ts:123`, `:148-151`).
    selected: Option<String>,
    focused: bool,
    events: Arc<Mutex<Vec<McpSelectorEvent>>>,
}

impl McpSelectorComponent {
    /// The constructor (`ui.ts:84-89`): the view starts on "Loading…".
    pub fn new(events: Arc<Mutex<Vec<McpSelectorEvent>>>) -> Self {
        Self {
            screen: McpSelectorScreen::Loading,
            list: None,
            input: Input::new(InputOptions::default()),
            selected: None,
            focused: false,
            events,
        }
    }

    /// `setContent` (`ui.ts:104-111`) followed by whatever list/input the
    /// new content needs. A `Menu` screen here begins a *new* `menu()`
    /// invocation, so the preserved selection resets; use
    /// [`Self::set_menu`] for a `subscribe` rebuild of the current one.
    pub fn set_screen(&mut self, screen: McpSelectorScreen) {
        if matches!(screen, McpSelectorScreen::Menu(_)) {
            self.selected = None;
        }
        match screen {
            McpSelectorScreen::Menu(menu) => self.set_menu(menu),
            other => {
                self.list = None;
                self.screen = other;
                if matches!(&self.screen, McpSelectorScreen::RedirectUrl { .. }) {
                    self.input.set_value("");
                    Focusable::set_focused(&mut self.input, self.focused);
                }
            }
        }
    }

    /// One `render()` of `menu()` (`ui.ts:124-156`): rebuild from the fresh
    /// menu, keeping the highlighted value (`wanted = selected ?? menu.selected`).
    pub fn set_menu(&mut self, menu: McpMenu) {
        let wanted = self.selected.clone().or_else(|| menu.selected.clone());
        self.list = build_list(&menu, wanted.as_deref());
        self.selected = self
            .list
            .as_ref()
            .and_then(SelectList::selected_item)
            .map(|item| item.value.clone());
        self.screen = McpSelectorScreen::Menu(menu);
    }

    /// The value the menu would resolve to right now.
    pub fn selected_value(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Take the events published since the last drain.
    pub fn drain(&self) -> Vec<McpSelectorEvent> {
        std::mem::take(&mut *self.events.lock().expect("mcp selector events"))
    }

    /// The menu body: details, error, a spacer, then either the empty message
    /// or the list (`ui.ts:127-155`).
    fn menu_body(&self, menu: &McpMenu, width: usize) -> Vec<String> {
        let palette = theme::theme();
        let mut body = Vec::new();
        if let Some(details) = &menu.details {
            for line in details.split('\n') {
                body.extend(text_lines(&palette.fg(ThemeColor::Muted, line), width));
            }
        }
        if let Some(error) = &menu.error {
            for line in error.split('\n') {
                body.extend(text_lines(&palette.fg(ThemeColor::Error, line), width));
            }
        }
        body.push(blank_line(width));
        if let Some(list) = &self.list {
            for line in list.render(width) {
                body.push(pad_line(&line, width));
            }
        } else {
            let empty = menu.empty.as_deref().unwrap_or("Nothing to show.");
            body.extend(text_lines(&palette.fg(ThemeColor::Muted, empty), width));
        }
        body
    }

    /// The menu footer (`ui.ts:131`, `:134`).
    fn menu_footer(menu: &McpMenu) -> String {
        if menu.items.is_empty() {
            key_hint(action::SELECT_CANCEL, &menu.cancel_label)
        } else {
            format!(
                "{} • {}",
                key_hint(action::SELECT_CONFIRM, &menu.confirm_label),
                key_hint(action::SELECT_CANCEL, &menu.cancel_label)
            )
        }
    }

    /// The sign-in body (`ui.ts:194-205`): `AuthUrlComponent`'s OSC-8 URL
    /// and "Ctrl+click to open • <key> to copy" hint are rendered inline.
    fn redirect_body(&self, authorization_url: &str, width: usize) -> Vec<String> {
        let palette = theme::theme();
        let mut body = vec![blank_line(width)];
        body.extend(text_lines(
            &palette.fg(
                ThemeColor::Muted,
                "Approve access in your browser. If it did not open, visit:",
            ),
            width,
        ));
        let link = hyperlink(authorization_url, authorization_url);
        body.extend(text_lines(&palette.fg(ThemeColor::Accent, &link), width));
        let click_hint = hyperlink("Ctrl+click to open", authorization_url);
        body.extend(text_lines(
            &format!(
                "{} {} {}",
                palette.fg(ThemeColor::Dim, &click_hint),
                palette.fg(ThemeColor::Dim, "•"),
                key_hint(theme::app_keys::MESSAGE_COPY, "to copy")
            ),
            width,
        ));
        body.push(blank_line(width));
        body.extend(text_lines(
            &palette.fg(
                ThemeColor::Muted,
                "If the browser runs on another machine, paste the URL it was redirected to:",
            ),
            width,
        ));
        for line in self.input.render(width) {
            body.push(pad_line(&line, width));
        }
        body
    }

    /// `handleInput` (`ui.ts:236-239`) plus the per-screen handlers
    /// (`ui.ts:135-138`, `:170-174`, `:213-230`).
    fn handle_screen_input(&mut self, data: &str) {
        match &self.screen {
            McpSelectorScreen::Loading => {}
            McpSelectorScreen::Menu(menu) => {
                if menu.items.is_empty() {
                    if keybindings::matches(data, action::SELECT_CANCEL) {
                        self.events
                            .lock()
                            .expect("mcp selector events")
                            .push(McpSelectorEvent::Cancel);
                    }
                    return;
                }
                let Some(list) = self.list.as_mut() else {
                    return;
                };
                match list.handle_key(data) {
                    SelectListAction::SelectionChanged => {
                        if let Some(item) = list.selected_item() {
                            let value = item.value.clone();
                            self.selected = Some(value.clone());
                            self.events
                                .lock()
                                .expect("mcp selector events")
                                .push(McpSelectorEvent::SelectionChanged(value));
                        }
                    }
                    SelectListAction::Confirmed => {
                        if let Some(item) = list.selected_item() {
                            self.events
                                .lock()
                                .expect("mcp selector events")
                                .push(McpSelectorEvent::Select(item.value.clone()));
                        }
                    }
                    SelectListAction::Cancelled => {
                        self.events
                            .lock()
                            .expect("mcp selector events")
                            .push(McpSelectorEvent::Cancel);
                    }
                    SelectListAction::None => {}
                }
            }
            McpSelectorScreen::Status { cancellable, .. } => {
                if *cancellable && keybindings::matches(data, action::SELECT_CANCEL) {
                    self.events
                        .lock()
                        .expect("mcp selector events")
                        .push(McpSelectorEvent::StatusCancelled);
                }
            }
            McpSelectorScreen::RedirectUrl {
                authorization_url, ..
            } => {
                if keybindings::matches(data, action::SELECT_CONFIRM) {
                    // Confirm is intercepted before the input, like upstream.
                    let value = self.input.value().trim().to_string();
                    if !value.is_empty() {
                        self.events
                            .lock()
                            .expect("mcp selector events")
                            .push(McpSelectorEvent::RedirectUrl(value));
                    }
                } else if keybindings::matches(data, action::SELECT_CANCEL) {
                    self.events
                        .lock()
                        .expect("mcp selector events")
                        .push(McpSelectorEvent::Cancel);
                } else if theme::app_keys::matches(data, theme::app_keys::MESSAGE_COPY) {
                    self.events
                        .lock()
                        .expect("mcp selector events")
                        .push(McpSelectorEvent::CopyUrl(authorization_url.clone()));
                } else {
                    let _ = self.input.handle_key(data);
                }
            }
        }
    }
}

impl Component for McpSelectorComponent {
    /// `frame()` + `render()` (`ui.ts:58-69`, `:241-245`).
    fn render(&self, width: usize) -> Vec<String> {
        let palette = theme::theme();
        let (title, body, footer): (String, Vec<String>, Option<String>) = match &self.screen {
            McpSelectorScreen::Loading => {
                let mut body = vec![blank_line(width)];
                body.extend(text_lines(
                    &palette.fg(ThemeColor::Muted, "Loading…"),
                    width,
                ));
                body.push(blank_line(width));
                ("MCP servers".to_string(), body, None)
            }
            McpSelectorScreen::Menu(menu) => (
                menu.title.clone(),
                self.menu_body(menu, width),
                Some(Self::menu_footer(menu)),
            ),
            McpSelectorScreen::Status {
                title,
                message,
                cancellable,
            } => {
                let mut body = vec![blank_line(width)];
                body.extend(text_lines(&palette.fg(ThemeColor::Muted, message), width));
                (
                    title.clone(),
                    body,
                    cancellable.then(|| key_hint(action::SELECT_CANCEL, "cancel")),
                )
            }
            McpSelectorScreen::RedirectUrl {
                title,
                authorization_url,
            } => (
                title.clone(),
                self.redirect_body(authorization_url, width),
                Some(format!(
                    "{} • {}",
                    key_hint(action::SELECT_CONFIRM, "submit"),
                    key_hint(action::SELECT_CANCEL, "cancel")
                )),
            ),
        };

        let mut lines = vec![border_line(width)];
        lines.extend(text_lines(
            &palette.fg(ThemeColor::Accent, &theme::bold(&title)),
            width,
        ));
        lines.extend(body);
        if let Some(footer) = footer {
            lines.push(blank_line(width));
            lines.extend(text_lines(&palette.fg(ThemeColor::Dim, &footer), width));
        }
        lines.push(border_line(width));
        lines
            .into_iter()
            .map(|line| {
                if visible_width(&line) > width {
                    truncate_to_width(&line, width, "", false)
                } else {
                    line
                }
            })
            .collect()
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        self.handle_screen_input(data);
        Some(ComponentMutation { consume: true })
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if matches!(&self.screen, McpSelectorScreen::RedirectUrl { .. }) {
            Focusable::set_focused(&mut self.input, focused);
        }
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    fn queue() -> Arc<Mutex<Vec<McpSelectorEvent>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn components(names: &[&str]) -> Vec<SelectItem> {
        names
            .iter()
            .map(|name| SelectItem::new(*name, *name))
            .collect()
    }

    fn servers_menu() -> McpMenu {
        let mut menu = McpMenu::new(
            "MCP servers",
            vec![
                SelectItem::new("alpha", "alpha").with_description("connected · tools · global"),
                SelectItem::new("beta", "beta").with_description("needs-auth · hidden · project"),
            ],
        );
        menu.empty =
            Some("No MCP servers configured. Add them to mcp.json or .pi/mcp.json.".into());
        menu.confirm_label = "manage".into();
        menu.cancel_label = "close".into();
        menu
    }

    fn plain(component: &McpSelectorComponent, width: usize) -> String {
        strip_terminal_sequences(&component.render(width).join("\n"))
    }

    #[test]
    fn loading_view_uses_the_upstream_frame() {
        let component = McpSelectorComponent::new(queue());
        let lines = plain(&component, 40);
        assert!(lines.contains("MCP servers"), "{lines}");
        assert!(lines.contains("Loading…"), "{lines}");
        let rendered = component.render(40);
        assert_eq!(rendered.len(), 6);
        assert_eq!(visible_width(&rendered[0]), 40);
        assert_eq!(visible_width(&rendered[5]), 40);
    }

    #[test]
    fn menu_renders_details_error_rows_and_hints() {
        let mut menu = servers_menu();
        menu.details = Some("command: npx".into());
        menu.error = Some("config: bad json\noverridden: beta".into());
        let mut component = McpSelectorComponent::new(queue());
        component.set_screen(McpSelectorScreen::Menu(menu));
        let lines = plain(&component, 70);
        assert!(lines.contains("MCP servers"), "{lines}");
        assert!(lines.contains("command: npx"), "{lines}");
        assert!(lines.contains("config: bad json"), "{lines}");
        assert!(lines.contains("overridden: beta"), "{lines}");
        assert!(lines.contains("→ alpha"), "{lines}");
        assert!(lines.contains("connected · tools · global"), "{lines}");
        assert!(
            lines.contains("enter manage • escape/ctrl+c close"),
            "{lines}"
        );
    }

    #[test]
    fn empty_menu_uses_empty_message_and_cancel_only_footer() {
        let mut menu = servers_menu();
        menu.items.clear();
        let mut component = McpSelectorComponent::new(queue());
        component.set_screen(McpSelectorScreen::Menu(menu));
        let lines = plain(&component, 70);
        assert!(
            lines.contains("No MCP servers configured. Add them to mcp.json or .pi/mcp.json."),
            "{lines}"
        );
        assert!(!lines.contains("enter manage"), "{lines}");
        assert!(lines.contains("escape/ctrl+c close"), "{lines}");
    }

    #[test]
    fn empty_menu_without_empty_text_falls_back_to_nothing_to_show() {
        let mut menu = servers_menu();
        menu.items.clear();
        menu.empty = None;
        let mut component = McpSelectorComponent::new(queue());
        component.set_screen(McpSelectorScreen::Menu(menu));
        assert!(plain(&component, 70).contains("Nothing to show."));
    }

    #[test]
    fn navigation_emits_selection_changes_and_enter_selects() {
        let events = queue();
        let mut component = McpSelectorComponent::new(Arc::clone(&events));
        component.set_screen(McpSelectorScreen::Menu(servers_menu()));
        component.handle_input("\x1b[B");
        assert_eq!(component.selected_value(), Some("beta"));
        component.handle_input("\r");
        let pushed = events.lock().expect("events").clone();
        assert_eq!(
            pushed,
            vec![
                McpSelectorEvent::SelectionChanged("beta".into()),
                McpSelectorEvent::Select("beta".into()),
            ]
        );
        assert_eq!(component.drain(), pushed);
        assert_eq!(*events.lock().expect("events"), vec![]);
    }

    #[test]
    fn escape_cancels_a_menu_and_an_empty_menu() {
        let events = queue();
        let mut component = McpSelectorComponent::new(Arc::clone(&events));
        component.set_screen(McpSelectorScreen::Menu(servers_menu()));
        component.handle_input("\x1b");
        let mut menu = servers_menu();
        menu.items.clear();
        component.set_screen(McpSelectorScreen::Menu(menu));
        component.handle_input("\x03");
        assert_eq!(
            component.drain(),
            vec![McpSelectorEvent::Cancel, McpSelectorEvent::Cancel]
        );
    }

    #[test]
    fn rebuild_keeps_the_highlighted_value_and_falls_back_to_menu_selected() {
        let mut component = McpSelectorComponent::new(queue());
        let mut menu = servers_menu();
        menu.selected = Some("beta".into());
        component.set_screen(McpSelectorScreen::Menu(menu));
        assert_eq!(component.selected_value(), Some("beta"));
        component.handle_input("\x1b[A");
        assert_eq!(component.selected_value(), Some("alpha"));

        // A subscribe rebuild with a fresh menu still lands on "alpha".
        component.set_menu(servers_menu());
        assert_eq!(component.selected_value(), Some("alpha"));

        // A new menu invocation (set_screen) resets to the menu's own pick.
        let mut next = servers_menu();
        next.selected = Some("beta".into());
        component.set_screen(McpSelectorScreen::Menu(next));
        assert_eq!(component.selected_value(), Some("beta"));
    }

    #[test]
    fn rebuild_where_the_value_disappeared_selects_the_first_item() {
        let mut component = McpSelectorComponent::new(queue());
        let mut menu = servers_menu();
        menu.selected = Some("beta".into());
        component.set_screen(McpSelectorScreen::Menu(menu));
        assert_eq!(component.selected_value(), Some("beta"));
        component.set_menu(McpMenu::new("MCP servers", components(&["gamma", "delta"])));
        assert_eq!(component.selected_value(), Some("gamma"));
        assert!(plain(&component, 70).contains("→ gamma"));
    }

    #[test]
    fn twelve_rows_cap_the_visible_window_and_add_scroll_info() {
        let names: Vec<String> = (0..20).map(|index| format!("server-{index:02}")).collect();
        let items = names
            .iter()
            .map(|name| SelectItem::new(name.clone(), name.clone()))
            .collect();
        let mut component = McpSelectorComponent::new(queue());
        component.set_screen(McpSelectorScreen::Menu(McpMenu::new("MCP servers", items)));
        let rows = component
            .render(60)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .filter(|line| line.starts_with("→ ") || line.starts_with("  server-"))
            .count();
        assert_eq!(rows, MAX_VISIBLE_ITEMS, "{rows}");
        assert!(plain(&component, 60).contains("(1/20)"));
    }

    #[test]
    fn status_renders_message_and_only_cancellable_screens_emit() {
        let events = queue();
        let mut component = McpSelectorComponent::new(Arc::clone(&events));
        component.set_screen(McpSelectorScreen::Status {
            title: "Sign in to alpha".into(),
            message: "Contacting the authorization server…".into(),
            cancellable: false,
        });
        let lines = plain(&component, 70);
        assert!(lines.contains("Sign in to alpha"), "{lines}");
        assert!(
            lines.contains("Contacting the authorization server…"),
            "{lines}"
        );
        assert!(!lines.contains("cancel"), "{lines}");
        component.handle_input("\x1b");
        assert_eq!(component.drain(), vec![]);

        component.set_screen(McpSelectorScreen::Status {
            title: "Sign in to alpha".into(),
            message: "Connecting…".into(),
            cancellable: true,
        });
        assert!(plain(&component, 70).contains("escape/ctrl+c cancel"));
        component.handle_input("\x1b");
        assert_eq!(component.drain(), vec![McpSelectorEvent::StatusCancelled]);
    }

    #[test]
    fn redirect_screen_renders_instructions_link_and_input() {
        let mut component = McpSelectorComponent::new(queue());
        component.set_screen(McpSelectorScreen::RedirectUrl {
            title: "Sign in to alpha".into(),
            authorization_url: "https://example.com/authorize".into(),
        });
        let lines = plain(&component, 80);
        assert!(lines.contains("Sign in to alpha"), "{lines}");
        assert!(
            lines.contains("Approve access in your browser. If it did not open, visit:"),
            "{lines}"
        );
        assert!(lines.contains("https://example.com/authorize"), "{lines}");
        assert!(lines.contains("Ctrl+click to open"), "{lines}");
        assert!(
            lines.contains(
                "If the browser runs on another machine, paste the URL it was redirected to:"
            ),
            "{lines}"
        );
        assert!(
            lines.contains("enter submit • escape/ctrl+c cancel"),
            "{lines}"
        );
    }

    #[test]
    fn redirect_screen_submit_copy_and_cancel_events() {
        let events = queue();
        let mut component = McpSelectorComponent::new(Arc::clone(&events));
        component.set_screen(McpSelectorScreen::RedirectUrl {
            title: "Sign in to alpha".into(),
            authorization_url: "https://example.com/authorize".into(),
        });
        // Confirm with an empty input resolves nothing, like upstream.
        component.handle_input("\r");
        assert_eq!(component.drain(), vec![]);
        for character in "  https://example.com/callback  ".chars() {
            component.handle_input(&character.to_string());
        }
        component.handle_input("\r");
        component.handle_input("\x18"); // ctrl+x = app.message.copy
        component.handle_input("\x1b");
        assert_eq!(
            component.drain(),
            vec![
                McpSelectorEvent::RedirectUrl("https://example.com/callback".into()),
                McpSelectorEvent::CopyUrl("https://example.com/authorize".into()),
                McpSelectorEvent::Cancel,
            ]
        );
    }

    #[test]
    fn resize_remeasures_the_border_and_clips_long_lines() {
        let mut menu = servers_menu();
        menu.error = Some("x".repeat(200));
        let mut component = McpSelectorComponent::new(queue());
        component.set_screen(McpSelectorScreen::Menu(menu));
        for width in [20, 100] {
            let lines = component.render(width);
            assert!(
                lines.iter().all(|line| visible_width(line) == width),
                "every line must be exactly {width} columns"
            );
        }
        assert!(plain(&component, 20).contains("MCP servers"));
    }
}
