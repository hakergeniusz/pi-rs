//! `TrustSelectorComponent` — the `/trust` dialog.
//!
//! Ground truth: `modes/interactive/components/trust-selector.ts`; the
//! options come from `getProjectTrustOptions` (ported in
//! [`crate::cli::project_trust`]). The `j`/`k` keys ride the upstream
//! handler's literal fallbacks. Events reach the orchestrator through the
//! queue the mode drains in its pump, like the model selector's.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pi_tui::component::{Component, ComponentMutation};
use pi_tui::keybindings;
use pi_tui::keybindings::action;
use pi_tui::theme::ThemeColor;

use crate::cli::project_trust::TrustOption;
use crate::modes::interactive::components::header::DynamicBorder;
use crate::modes::interactive::theme;

/// Events the selector publishes to the orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub enum TrustSelectorEvent {
    /// `onSelect` — `{ trusted, updates }` from the highlighted option.
    Select {
        trusted: bool,
        updates: Vec<(PathBuf, Option<bool>)>,
    },
    /// `onCancel` — escape/ctrl+c.
    Cancel,
}

/// `formatDecision` (`trust-selector.ts:8-16`).
fn format_decision(trust_path: Option<&str>, decision: Option<(String, bool)>) -> String {
    let Some((path, trusted)) = decision else {
        return "none".to_string();
    };
    let label = if trusted { "trusted" } else { "untrusted" };
    if trust_path.is_some_and(|saved_path| saved_path != path) {
        format!("{label} (inherited from {path})")
    } else {
        format!("{label} ({path})")
    }
}

/// The dialog (`TrustSelectorComponent`, `trust-selector.ts:18-`).
pub struct TrustSelectorComponent {
    cwd: String,
    saved_decision: Option<(String, bool)>,
    project_trusted: bool,
    options: Vec<TrustOption>,
    selected_index: usize,
    events: Arc<Mutex<Vec<TrustSelectorEvent>>>,
}

impl TrustSelectorComponent {
    /// The constructor (`trust-selector.ts:25-48`): the initial selection is
    /// the option matching the saved decision, else the first.
    pub fn new(
        cwd: &std::path::Path,
        saved_decision: Option<(String, bool)>,
        project_trusted: bool,
        events: Arc<Mutex<Vec<TrustSelectorEvent>>>,
    ) -> Self {
        let options = crate::cli::project_trust::project_trust_options(cwd);
        let selected_index = options
            .iter()
            .position(|option| {
                option.saved_path.as_deref().is_some_and(|saved_path| {
                    saved_decision.as_ref().is_some_and(|(path, decision)| {
                        *decision == option.trusted && path == saved_path
                    })
                })
            })
            .unwrap_or(0);
        TrustSelectorComponent {
            cwd: cwd.to_string_lossy().to_string(),
            saved_decision,
            project_trusted,
            options,
            selected_index,
            events,
        }
    }

    /// `updateList`'s option lines (`trust-selector.ts:58-76`).
    fn option_lines(&self) -> Vec<String> {
        let theme = theme::theme();
        self.options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let is_selected = index == self.selected_index;
                let is_current = option.saved_path.as_deref().is_some_and(|saved_path| {
                    self.saved_decision
                        .as_ref()
                        .is_some_and(|(path, decision)| {
                            *decision == option.trusted && path == saved_path
                        })
                });
                let current_marker = if is_current {
                    theme.fg(ThemeColor::Accent, "✓ ")
                } else {
                    "  ".to_string()
                };
                let prefix = if is_selected {
                    theme.fg(ThemeColor::Accent, "→ ")
                } else {
                    "  ".to_string()
                };
                let label = if is_selected {
                    theme.fg(ThemeColor::Accent, &option.label)
                } else {
                    theme.fg(ThemeColor::Text, &option.label)
                };
                format!("{prefix}{current_marker}{label}")
            })
            .collect()
    }
}

impl Component for TrustSelectorComponent {
    fn render(&self, _width: usize) -> Vec<String> {
        let theme = theme::theme();
        let hint = format!(
            "{}  {}  {}",
            theme::raw_key_hint("↑↓", "navigate"),
            theme::key_hint(action::SELECT_CONFIRM, "save"),
            theme::key_hint(action::SELECT_CANCEL, "cancel"),
        );
        let border = DynamicBorder::new();
        let mut lines = Vec::new();
        lines.extend(border.render(0));
        lines.push(String::new());
        lines.push(theme.fg(ThemeColor::Accent, &theme::bold("Project trust")));
        lines.push(theme.fg(ThemeColor::Muted, &self.cwd));
        lines.push(String::new());
        lines.push(theme.fg(
            ThemeColor::Muted,
            &format!(
                "Saved decision: {}",
                format_decision(
                    self.options
                        .first()
                        .and_then(|option| option.saved_path.as_deref()),
                    self.saved_decision.clone(),
                )
            ),
        ));
        lines.push(theme.fg(
            ThemeColor::Muted,
            &format!(
                "Current session: {}",
                if self.project_trusted {
                    "trusted"
                } else {
                    "untrusted"
                }
            ),
        ));
        lines.push(String::new());
        lines.extend(self.option_lines());
        lines.push(String::new());
        lines.push(theme.fg(ThemeColor::Muted, &hint));
        lines.push(String::new());
        lines.extend(border.render(0));
        lines
    }

    /// `handleInput` (`trust-selector.ts:78-99`).
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        if keybindings::matches(data, action::SELECT_UP) || data == "k" {
            self.selected_index = self.selected_index.saturating_sub(1);
        } else if keybindings::matches(data, action::SELECT_DOWN) || data == "j" {
            self.selected_index = (self.selected_index + 1).min(self.options.len() - 1);
        } else if keybindings::matches(data, action::SELECT_CONFIRM) || data == "\n" {
            if let Some(selected) = self.options.get(self.selected_index) {
                self.events
                    .lock()
                    .expect("trust events")
                    .push(TrustSelectorEvent::Select {
                        trusted: selected.trusted,
                        updates: selected.updates.clone(),
                    });
            }
        } else if keybindings::matches(data, action::SELECT_CANCEL) {
            self.events
                .lock()
                .expect("trust events")
                .push(TrustSelectorEvent::Cancel);
        } else {
            return None;
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
    use std::path::Path;

    fn selector(dir: &std::path::Path) -> TrustSelectorComponent {
        TrustSelectorComponent::new(dir, None, false, Arc::new(Mutex::new(Vec::new())))
    }

    #[test]
    fn options_are_trust_parent_and_distrust() {
        // Only the filesystem root has no parent folder to offer.
        let root_options = crate::cli::project_trust::project_trust_options(Path::new("/"));
        assert_eq!(root_options.len(), 2);
        assert_eq!(root_options[0].label, "Trust");
        assert_eq!(root_options[1].label, "Do not trust");

        let root = tempfile::tempdir().expect("tempdir");
        let project = root.path().join("code").join("app");
        std::fs::create_dir_all(&project).expect("mkdir");
        let options = crate::cli::project_trust::project_trust_options(&project);
        assert_eq!(options.len(), 3);
        assert_eq!(
            options[1].label,
            format!(
                "Trust parent folder ({})",
                crate::cli::project_trust::project_trust_parent_path(&project).expect("parent")
            )
        );
        // Confirming the parent option deletes the project's own entry so
        // the parent's decision applies.
        assert_eq!(options[1].updates[1], (project.clone(), None));
    }

    #[test]
    fn render_shows_the_saved_decision_and_current_session_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let component = selector(dir.path());
        let plain = strip_terminal_sequences(&component.render(100).join("\n"));
        assert!(plain.contains("Project trust"), "{plain}");
        assert!(
            plain.contains(&dir.path().to_string_lossy().to_string()),
            "{plain}"
        );
        assert!(plain.contains("Saved decision: none"), "{plain}");
        assert!(plain.contains("Current session: untrusted"), "{plain}");
        assert!(plain.contains("→   Trust"), "{plain}");
        assert!(plain.contains("  Do not trust"), "{plain}");
    }

    #[test]
    fn navigation_clamps_and_confirm_emits_the_selection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut component =
            TrustSelectorComponent::new(dir.path(), None, false, Arc::clone(&events));
        // Up one from the first option is a no-op; down twice lands on
        // "Do not trust"; further down clamps.
        component.handle_input("\x1b[A").expect("up handled");
        component.handle_input("\x1b[B").expect("down handled");
        component.handle_input("\x1b[B").expect("down handled");
        component.handle_input("\x1b[B").expect("down clamped");
        component.handle_input("\r").expect("confirm handled");
        let pushed = std::mem::take(&mut *events.lock().expect("events"));
        assert_eq!(
            pushed,
            vec![TrustSelectorEvent::Select {
                trusted: false,
                updates: vec![(dir.path().to_path_buf(), Some(false))]
            }]
        );
    }

    #[test]
    fn vim_keys_and_cancel_work_like_upstream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut component =
            TrustSelectorComponent::new(dir.path(), None, false, Arc::clone(&events));
        component.handle_input("j").expect("j handled");
        component.handle_input("\x1b").expect("escape handled");
        let pushed = std::mem::take(&mut *events.lock().expect("events"));
        assert_eq!(pushed, vec![TrustSelectorEvent::Cancel]);
    }

    #[test]
    fn the_saved_option_is_preselected() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The trust path the options builder would record.
        let saved_path = crate::cli::project_trust::project_trust_options(dir.path())[0]
            .saved_path
            .clone()
            .expect("saved path");
        let component = TrustSelectorComponent::new(
            dir.path(),
            Some((saved_path, true)),
            true,
            Arc::new(Mutex::new(Vec::new())),
        );
        let plain = strip_terminal_sequences(&component.render(100).join("\n"));
        assert!(plain.contains("→ ✓ Trust"), "{plain}");
        assert!(plain.contains("Saved decision: trusted"), "{plain}");
        assert!(plain.contains("Current session: trusted"), "{plain}");
    }
}
