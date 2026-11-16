//! `OAuthSelectorComponent` and the `/login` input prompt.
//!
//! Ground truth: `modes/interactive/components/oauth-selector.ts` (the
//! provider selector: search input, fuzzy filter, `MAX_VISIBLE` windowing,
//! `updateList`'s row assembly `:175-228`, `handleInput` `:230-264`, the
//! empty-state messages `:219-225`) and the
//! `LoginDialogComponent.showPrompt` subset (`login-dialog.ts:229-257`) the
//! API-key flow drives. The plain-option rows stand in for upstream's
//! `ExtensionSelectorComponent` (the generic list) in the auth-type menu;
//! the `RadiusLoginMenuComponent` brand shimmer
//! (`radius-login-selector.ts:44-58`) is not ported. Events reach the
//! orchestrator through the queue the mode drains in its pump, like the
//! other selectors'.

use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::input::{Input, InputAction, InputOptions};
use pi_tui::keybindings;
use pi_tui::keybindings::action;
use pi_tui::theme::ThemeColor;

use crate::modes::interactive::components::header::DynamicBorder;
use crate::modes::interactive::theme;
use crate::oauth::{self, AuthSelectorProvider};

/// `MAX_VISIBLE` (`oauth-selector.ts:28`).
const MAX_VISIBLE: usize = 8;

/// Events the selector publishes to the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSelectorEvent {
    /// `onSelect(providerId, authType)` — a provider row confirmed.
    Select {
        provider_id: String,
        /// `"oauth" | "api_key"`.
        auth_type: &'static str,
    },
    /// `onSelect(option)` — a plain option row confirmed (the auth-type
    /// menu and the logout rows' string form).
    SelectOption(String),
    /// `onCancel` — escape/ctrl+c.
    Cancel,
}

/// One selectable row: a provider entry or a plain option string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthSelectorRow {
    Provider(AuthSelectorProvider),
    Option(String),
}

impl AuthSelectorRow {
    /// `filterProviders`' search text (`oauth-selector.ts:138-145`): the
    /// provider name, id, auth type and method name; plain options filter
    /// on themselves.
    fn search_text(&self) -> String {
        match self {
            AuthSelectorRow::Provider(provider) => format!(
                "{} {} {} {}",
                provider.name,
                provider.id,
                provider.auth_type,
                provider.method_name.as_deref().unwrap_or(""),
            ),
            AuthSelectorRow::Option(option) => option.clone(),
        }
    }

    fn select_event(&self) -> AuthSelectorEvent {
        match self {
            AuthSelectorRow::Provider(provider) => AuthSelectorEvent::Select {
                provider_id: provider.id.clone(),
                auth_type: provider.auth_type,
            },
            AuthSelectorRow::Option(option) => AuthSelectorEvent::SelectOption(option.clone()),
        }
    }
}

/// The provider/option selector (`OAuthSelectorComponent`,
/// `oauth-selector.ts:47-`, and the `ExtensionSelectorComponent` stand-in
/// for the auth-type menu).
pub struct AuthSelectorComponent {
    /// `"login" | "logout"` — picks the empty-list message
    /// (`oauth-selector.ts:219-225`).
    mode: &'static str,
    title: String,
    rows: Vec<AuthSelectorRow>,
    /// Whether the `[subscription]`/`[API key]` labels render
    /// (`showAuthTypeLabels`: more than one auth type present).
    show_auth_type_labels: bool,
    filtered: Vec<AuthSelectorRow>,
    selected_index: usize,
    search_input: Input,
    events: Arc<Mutex<Vec<AuthSelectorEvent>>>,
}

impl AuthSelectorComponent {
    /// The login constructor (`oauth-selector.ts:62-112`): provider rows,
    /// the upstream title, optional initial search text.
    pub fn login(
        providers: Vec<AuthSelectorProvider>,
        initial_search: Option<&str>,
        events: Arc<Mutex<Vec<AuthSelectorEvent>>>,
    ) -> Self {
        let show_auth_type_labels = providers
            .iter()
            .map(|provider| provider.auth_type)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1;
        let rows = providers
            .into_iter()
            .map(AuthSelectorRow::Provider)
            .collect();
        Self::new(
            "login",
            "Select provider to configure:",
            rows,
            show_auth_type_labels,
            initial_search,
            events,
        )
    }

    /// The logout constructor (`mode === "logout"`, `oauth-selector.ts:96-98`).
    pub fn logout(
        providers: Vec<AuthSelectorProvider>,
        events: Arc<Mutex<Vec<AuthSelectorEvent>>>,
    ) -> Self {
        let show_auth_type_labels = providers
            .iter()
            .map(|provider| provider.auth_type)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 1;
        let rows = providers
            .into_iter()
            .map(AuthSelectorRow::Provider)
            .collect();
        Self::new(
            "logout",
            "Select provider to logout:",
            rows,
            show_auth_type_labels,
            None,
            events,
        )
    }

    /// The plain-option constructor (the `ExtensionSelectorComponent` menus:
    /// `showLoginAuthTypeSelector`'s auth-type list, `interactive-mode.ts:6087-6155`).
    pub fn options(
        title: &str,
        options: Vec<String>,
        events: Arc<Mutex<Vec<AuthSelectorEvent>>>,
    ) -> Self {
        let rows: Vec<AuthSelectorRow> = options.into_iter().map(AuthSelectorRow::Option).collect();
        Self::new("login", title, rows, false, None, events)
    }

    fn new(
        mode: &'static str,
        title: &str,
        rows: Vec<AuthSelectorRow>,
        show_auth_type_labels: bool,
        initial_search: Option<&str>,
        events: Arc<Mutex<Vec<AuthSelectorEvent>>>,
    ) -> Self {
        let mut search_input = Input::new(InputOptions::default());
        if let Some(initial) = initial_search {
            search_input.set_value(initial);
        }
        let mut selector = AuthSelectorComponent {
            mode,
            title: title.to_string(),
            show_auth_type_labels,
            filtered: Vec::new(),
            selected_index: 0,
            rows,
            search_input,
            events,
        };
        let initial = selector.search_input.value().to_string();
        selector.filter(&initial);
        selector
    }

    /// `filterProviders` (`oauth-selector.ts:137-148`).
    fn filter(&mut self, query: &str) {
        self.filtered = if query.trim().is_empty() {
            self.rows.clone()
        } else {
            pi_tui::fuzzy::fuzzy_filter(&self.rows, query, AuthSelectorRow::search_text)
        };
        self.selected_index = self
            .selected_index
            .min(self.filtered.len().saturating_sub(1));
    }

    /// `updateList`'s row line (`oauth-selector.ts:160-217`): the selection
    /// prefix, the partner ✦ marker, the optional auth-type label, and the
    /// status suffix.
    fn row_line(&self, row: &AuthSelectorRow, is_selected: bool) -> String {
        let theme = theme::theme();
        let accent = |text: &str| theme.fg(ThemeColor::Accent, text);
        let text_color = |text: &str| theme.fg(ThemeColor::Text, text);
        match row {
            AuthSelectorRow::Provider(provider) => {
                let status_indicator = provider.status.as_ref().map_or(
                    theme.fg(ThemeColor::Muted, " • not configured"),
                    |status| oauth::format_auth_selector_provider_status(provider, status),
                );
                // U+2726 is single-width in every terminal, unlike star
                // emoji (`oauth-selector.ts:166-168`).
                let partner_marker = if oauth::is_preferred_provider(&provider.id) {
                    theme.fg(ThemeColor::Muted, " ✦")
                } else {
                    String::default()
                };
                let auth_type_label = if self.show_auth_type_labels {
                    theme.fg(
                        ThemeColor::Muted,
                        &format!(
                            " [{}]",
                            oauth::format_auth_selector_provider_type(
                                provider.auth_type,
                                provider.subscription,
                            )
                        ),
                    )
                } else {
                    String::new()
                };
                let name = if is_selected {
                    accent(&provider.name)
                } else {
                    format!("  {}", text_color(&provider.name))
                };
                let prefix = if is_selected {
                    accent("→ ")
                } else {
                    String::new()
                };
                format!("{prefix}{name}{partner_marker}{auth_type_label}{status_indicator}")
            }
            AuthSelectorRow::Option(option) => {
                if is_selected {
                    format!("{}{}", accent("→ "), accent(option))
                } else {
                    format!("  {}", text_color(option))
                }
            }
        }
    }
}

impl Component for AuthSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let muted = |text: &str| theme.fg(ThemeColor::Muted, text);
        let hint = format!(
            "{}  {}  {}",
            theme::raw_key_hint("↑↓", "navigate"),
            theme::key_hint(action::SELECT_CONFIRM, "select"),
            theme::key_hint(action::SELECT_CANCEL, "cancel"),
        );
        let border = DynamicBorder::new();
        let mut lines = Vec::new();
        lines.extend(border.render(width));
        lines.push(String::new());
        lines.push(theme.fg(ThemeColor::Accent, &theme::bold(&self.title)));
        lines.push(String::new());
        lines.extend(self.search_input.render(width));
        lines.push(String::new());

        // The `MAX_VISIBLE` window centered on the selection
        // (`updateList`, `oauth-selector.ts:162-169`).
        let start_index = self.selected_index.saturating_sub(MAX_VISIBLE / 2).min(
            self.filtered
                .len()
                .saturating_sub(MAX_VISIBLE.min(self.filtered.len())),
        );
        let end_index = (start_index + MAX_VISIBLE).min(self.filtered.len());
        for index in start_index..end_index {
            let line = self.row_line(&self.filtered[index], index == self.selected_index);
            lines.push(pi_tui::ansi::truncate_to_width(&line, width, "", false));
        }
        // The scroll indicator is its own line on a clipped list
        // (`oauth-selector.ts:219-223`).
        if start_index > 0 || end_index < self.filtered.len() {
            lines.push(pi_tui::ansi::truncate_to_width(
                &muted(&format!(
                    "  ({}/{})",
                    self.selected_index + 1,
                    self.filtered.len()
                )),
                width,
                "",
                false,
            ));
        }
        if self.filtered.is_empty() {
            let message = if self.rows.is_empty() {
                if self.mode == "login" {
                    "No providers available"
                } else {
                    "No providers logged in. Use /login first."
                }
            } else {
                "No matching providers"
            };
            lines.push(muted(&format!("  {message}")));
        }
        lines.push(String::new());
        lines.push(muted(&hint));
        lines.push(String::new());
        lines.extend(border.render(width));
        lines
    }

    /// `handleInput` (`oauth-selector.ts:230-264`): navigation keys first,
    /// everything else goes to the search input.
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        let confirm = |selector: &mut Self| {
            if let Some(row) = selector.filtered.get(selector.selected_index) {
                let event = row.select_event();
                selector
                    .events
                    .lock()
                    .expect("auth selector events")
                    .push(event);
            }
        };
        if keybindings::matches(data, action::SELECT_UP) {
            if self.filtered.is_empty() {
                return Some(ComponentMutation { consume: true });
            }
            self.selected_index = self.selected_index.saturating_sub(1);
        } else if keybindings::matches(data, action::SELECT_DOWN) {
            if self.filtered.is_empty() {
                return Some(ComponentMutation { consume: true });
            }
            self.selected_index = (self.selected_index + 1).min(self.filtered.len() - 1);
        } else if keybindings::matches(data, action::SELECT_PAGE_UP) {
            if self.filtered.is_empty() {
                return Some(ComponentMutation { consume: true });
            }
            self.selected_index = self.selected_index.saturating_sub(MAX_VISIBLE);
        } else if keybindings::matches(data, action::SELECT_PAGE_DOWN) {
            if self.filtered.is_empty() {
                return Some(ComponentMutation { consume: true });
            }
            self.selected_index = (self.selected_index + MAX_VISIBLE).min(self.filtered.len() - 1);
        } else if keybindings::matches(data, action::SELECT_CONFIRM) {
            confirm(self);
        } else if keybindings::matches(data, action::SELECT_CANCEL) {
            self.events
                .lock()
                .expect("auth selector events")
                .push(AuthSelectorEvent::Cancel);
        } else {
            match self.search_input.handle_key(data) {
                InputAction::Submitted(_) => confirm(self),
                InputAction::Escaped => {
                    self.events
                        .lock()
                        .expect("auth selector events")
                        .push(AuthSelectorEvent::Cancel);
                }
                InputAction::None => {
                    let query = self.search_input.value().to_string();
                    self.filter(&query);
                    return None;
                }
            }
        }
        Some(ComponentMutation { consume: true })
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// Events the API-key input prompt publishes (`LoginDialogComponent`'s
/// `inputResolver`/`inputRejecter`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginPromptEvent {
    /// The entered value (`showPrompt`'s resolve).
    Submit(String),
    /// Escape/ctrl+c — upstream rejects with `Login cancelled`.
    Cancel,
}

/// The API-key entry dialog (`LoginDialogComponent`'s `showPrompt` subset,
/// `login-dialog.ts:229-257`): the message, the optional `e.g.` placeholder,
/// the input, and the submit/cancel hint.
pub struct LoginPromptComponent {
    message: String,
    placeholder: Option<String>,
    input: Input,
    events: Arc<Mutex<Vec<LoginPromptEvent>>>,
}

impl LoginPromptComponent {
    /// `showPrompt(message, placeholder)` (`login-dialog.ts:229-233`); the
    /// input starts empty each time (`:251`).
    pub fn new(
        message: &str,
        placeholder: Option<&str>,
        events: Arc<Mutex<Vec<LoginPromptEvent>>>,
    ) -> Self {
        let input = Input::new(InputOptions {
            prompt: String::new(),
            ..InputOptions::default()
        });
        LoginPromptComponent {
            message: message.to_string(),
            placeholder: placeholder.map(str::to_string),
            input,
            events,
        }
    }
}

impl Component for LoginPromptComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let border = DynamicBorder::new();
        let mut lines = Vec::new();
        lines.extend(border.render(width));
        lines.push(String::new());
        lines.push(theme.fg(ThemeColor::Text, &self.message));
        if let Some(placeholder) = &self.placeholder {
            lines.push(theme.fg(ThemeColor::Muted, &format!("e.g., {placeholder}")));
        }
        lines.extend(self.input.render(width));
        lines.push(theme.fg(
            ThemeColor::Muted,
            &format!(
                "({}, {})",
                theme::key_hint(action::SELECT_CANCEL, "to cancel,"),
                theme::key_hint(action::SELECT_CONFIRM, "to submit"),
            ),
        ));
        lines.push(String::new());
        lines.extend(border.render(width));
        lines
    }

    /// The dialog input's contract (`input.ts:335-348` via the dialog's
    /// `onSubmit`): enter submits, escape cancels.
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        match self.input.handle_key(data) {
            InputAction::Submitted(value) => {
                self.events
                    .lock()
                    .expect("login prompt events")
                    .push(LoginPromptEvent::Submit(value));
            }
            InputAction::Escaped => {
                self.events
                    .lock()
                    .expect("login prompt events")
                    .push(LoginPromptEvent::Cancel);
            }
            InputAction::None => return None,
        }
        Some(ComponentMutation { consume: true })
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;

    fn provider(
        id: &str,
        name: &str,
        auth_type: &'static str,
        configured: bool,
    ) -> AuthSelectorProvider {
        AuthSelectorProvider {
            id: id.to_string(),
            name: name.to_string(),
            auth_type,
            subscription: auth_type == "oauth",
            method_name: Some(format!("{name} auth")),
            login_label: None,
            status: configured.then_some(oauth::AuthStatus {
                configured: true,
                auth_type,
                source: Some("stored"),
                label: None,
            }),
        }
    }

    #[test]
    fn login_renders_the_upstream_title_and_rows() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = AuthSelectorComponent::login(
            vec![
                provider("radius", "Radius", "oauth", false),
                provider("openai", "OpenAI", "api_key", true),
            ],
            None,
            events,
        );
        let plain = strip_terminal_sequences(&selector.render(100).join("\n"));
        assert!(plain.contains("Select provider to configure:"), "{plain}");
        assert!(plain.contains("OpenAI"), "{plain}");
        assert!(plain.contains("✓ stored"), "{plain}");
        assert!(plain.contains("not configured"), "{plain}");
        // Mixed auth types show the labels (`showAuthTypeLabels`).
        assert!(plain.contains("[subscription]"), "{plain}");
        assert!(plain.contains("[API key]"), "{plain}");
    }

    #[test]
    fn confirm_emits_the_provider_selection() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut selector = AuthSelectorComponent::login(
            vec![provider("openai", "OpenAI", "api_key", true)],
            None,
            Arc::clone(&events),
        );
        selector.handle_input("\r").expect("confirm handled");
        let pushed = std::mem::take(&mut *events.lock().expect("events"));
        assert_eq!(
            pushed,
            vec![AuthSelectorEvent::Select {
                provider_id: "openai".to_string(),
                auth_type: "api_key",
            }]
        );
    }

    #[test]
    fn cancel_and_search_filter_work_like_upstream() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut selector = AuthSelectorComponent::login(
            vec![
                provider("openai", "OpenAI", "api_key", true),
                provider("groq", "Groq", "api_key", false),
            ],
            None,
            Arc::clone(&events),
        );
        selector.handle_input("\x1b").expect("escape handled");
        assert_eq!(
            std::mem::take(&mut *events.lock().expect("events")),
            vec![AuthSelectorEvent::Cancel]
        );
        // Typing filters the rows; a non-matching query shows the empty state.
        for ch in "gro".chars() {
            // Typed keys filter without consuming (`InputAction::None`).
            let _ = selector.handle_input(&ch.to_string());
        }
        let plain = strip_terminal_sequences(&selector.render(100).join("\n"));
        assert!(plain.contains("Groq"), "{plain}");
        assert!(!plain.contains("OpenAI"), "{plain}");
        selector.handle_input("zzz").expect("typed");
        let plain = strip_terminal_sequences(&selector.render(100).join("\n"));
        assert!(plain.contains("No matching providers"), "{plain}");
    }

    #[test]
    fn logout_uses_the_upstream_title() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = AuthSelectorComponent::logout(
            vec![provider("radius", "Radius", "oauth", true)],
            events,
        );
        let plain = strip_terminal_sequences(&selector.render(100).join("\n"));
        assert!(plain.contains("Select provider to logout:"), "{plain}");
    }

    #[test]
    fn the_prompt_submits_and_cancels() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut prompt = LoginPromptComponent::new(
            "Enter Anthropic API key",
            Some("sk-ant-..."),
            Arc::clone(&events),
        );
        let plain = strip_terminal_sequences(&prompt.render(100).join("\n"));
        assert!(plain.contains("Enter Anthropic API key"), "{plain}");
        assert!(plain.contains("e.g., sk-ant-..."), "{plain}");
        for ch in "sk-test".chars() {
            // Typed keys are consumed by the input, not the action return.
            let _ = prompt.handle_input(&ch.to_string());
        }
        prompt.handle_input("\r").expect("submit");
        assert_eq!(
            std::mem::take(&mut *events.lock().expect("events")),
            vec![LoginPromptEvent::Submit("sk-test".to_string())]
        );
        prompt.handle_input("\x1b").expect("cancel");
        assert_eq!(
            std::mem::take(&mut *events.lock().expect("events")),
            vec![LoginPromptEvent::Cancel]
        );
    }

    #[test]
    fn the_option_menu_selects_plain_strings() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut selector = AuthSelectorComponent::options(
            "Select authentication method:",
            vec![
                "Sign in with an account".to_string(),
                "Sign in with an API key".to_string(),
            ],
            Arc::clone(&events),
        );
        selector.handle_input("\x1b[B").expect("down");
        selector.handle_input("\r").expect("confirm");
        assert_eq!(
            std::mem::take(&mut *events.lock().expect("events")),
            vec![AuthSelectorEvent::SelectOption(
                "Sign in with an API key".to_string()
            )]
        );
    }

    #[test]
    fn logout_empty_state_says_use_login_first() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = AuthSelectorComponent::logout(Vec::new(), events);
        let plain = strip_terminal_sequences(&selector.render(100).join("\n"));
        assert!(
            plain.contains("No providers logged in. Use /login first."),
            "{plain}"
        );
    }
}
