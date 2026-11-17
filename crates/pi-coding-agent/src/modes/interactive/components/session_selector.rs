//! `SessionSelectorComponent` — the `/resume` dialog.
//!
//! Ground truth: `modes/interactive/components/session-selector.ts` (884
//! lines) with the search from [`super::session_selector_search`]. Deviations:
//! the loaders are synchronous (`SessionManager::list`/`list_all` — no
//! progress/abort plumbing), status messages persist until the next state
//! change instead of auto-hiding on a timer, and the upstream loader
//! callbacks collapse into direct calls because this component owns its
//! header and list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use pi_tui::ansi::{truncate_to_width, visible_width};
use pi_tui::component::{Component, ComponentMutation, Focusable};
use pi_tui::components::input::{Input, InputAction, InputOptions};
use pi_tui::keybindings;
use pi_tui::keybindings::action;
use pi_tui::theme::{ThemeBg, ThemeColor};

use super::session_selector_search::{
    NameFilter, SortMode, filter_and_sort_sessions, has_session_name,
};
use crate::session::store::{SessionInfo, SessionManager};

use super::header::DynamicBorder;
use crate::modes::interactive::theme;

/// Events the selector publishes to the orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionSelectorEvent {
    /// `onSelect(sessionPath)` — resume the highlighted session.
    Selected(PathBuf),
    /// `onCancel` — escape/ctrl+c.
    Cancel,
}

/// `formatSessionDate` (`session-selector.ts:11-28`).
fn format_session_date(modified: DateTime<Utc>) -> String {
    let diff = Utc::now().signed_duration_since(modified);
    let minutes = diff.num_minutes();
    if minutes < 1 {
        return "now".to_string();
    }
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = diff.num_hours();
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = diff.num_days();
    if days < 7 {
        return format!("{days}d");
    }
    if days < 30 {
        return format!("{}w", days / 7);
    }
    if days < 365 {
        return format!("{}mo", days / 30);
    }
    format!("{}y", days / 365)
}

/// `getSessionPathKey` (`session-selector.ts:31-33`): canonicalize so the
/// tree and current-session matching follow symlinks. (Upstream also
/// lowercases on Windows; the port is POSIX-only.)
fn session_path_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A display row: which session, and where it sits in the fork tree
/// (`flattenSessionTree` items, `session-selector.ts:249-256`).
#[derive(Debug, Clone)]
struct SessionNode {
    session_index: usize,
    depth: usize,
    is_last: bool,
    ancestor_continues: Vec<bool>,
}

/// `buildSessionTree` + `flattenSessionTree` (`session-selector.ts:190-256`):
/// group sessions by `parentSessionPath` (present ones only), sort every
/// level by latest subtree activity, and flatten with tree metadata.
fn flatten_session_tree(sessions: &[SessionInfo], subset: &[usize]) -> Vec<SessionNode> {
    let keys: HashMap<usize, PathBuf> = subset
        .iter()
        .map(|&index| (index, session_path_key(&sessions[index].path)))
        .collect();
    let index_of_key: HashMap<&Path, usize> = subset
        .iter()
        .filter_map(|&index| keys.get(&index).map(|key| (key.as_path(), index)))
        .collect();
    let mut children_map: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for &index in subset {
        let parent = sessions[index]
            .parent_session_path
            .as_deref()
            .map(|parent| session_path_key(Path::new(parent)));
        match parent.as_deref().and_then(|key| index_of_key.get(key)) {
            Some(&parent_index) => children_map.entry(parent_index).or_default().push(index),
            None => roots.push(index),
        }
    }
    sort_by_activity(&mut roots, sessions, &mut children_map);
    let mut out = Vec::new();
    for (position, &root) in roots.iter().enumerate() {
        walk_tree(
            &mut out,
            &children_map,
            root,
            0,
            &[],
            position == roots.len() - 1,
        );
    }
    out
}

/// The latest activity in a subtree (`updateLatestActivity`).
fn tree_latest_activity(
    sessions: &[SessionInfo],
    children_map: &HashMap<usize, Vec<usize>>,
    index: usize,
) -> i64 {
    let mut latest = sessions[index].modified.timestamp_millis();
    if let Some(children) = children_map.get(&index) {
        for &child in children {
            latest = latest.max(tree_latest_activity(sessions, children_map, child));
        }
    }
    latest
}

/// Sort nodes (and every child list) by latest subtree activity, descending
/// (`sortNodes`).
fn sort_by_activity(
    nodes: &mut [usize],
    sessions: &[SessionInfo],
    children_map: &mut HashMap<usize, Vec<usize>>,
) {
    nodes.sort_by(|&a, &b| {
        tree_latest_activity(sessions, children_map, b).cmp(&tree_latest_activity(
            sessions,
            children_map,
            a,
        ))
    });
    // Clone each children list out before re-sorting it: the recursion
    // needs the map again.
    for &node in nodes.iter() {
        if let Some(children) = children_map.get(&node).cloned() {
            let mut children = children;
            sort_by_activity(&mut children, sessions, children_map);
            if let Some(slot) = children_map.get_mut(&node) {
                *slot = children;
            }
        }
    }
}

/// The depth-first flatten (`walk`): depth, last-sibling flag, and which
/// ancestors continue, per row.
fn walk_tree(
    out: &mut Vec<SessionNode>,
    children_map: &HashMap<usize, Vec<usize>>,
    index: usize,
    depth: usize,
    ancestor_continues: &[bool],
    is_last: bool,
) {
    out.push(SessionNode {
        session_index: index,
        depth,
        is_last,
        ancestor_continues: ancestor_continues.to_vec(),
    });
    if let Some(children) = children_map.get(&index) {
        for (position, &child) in children.iter().enumerate() {
            let child_is_last = position == children.len() - 1;
            // Only show the continuation line for non-root ancestors.
            let continues = depth > 0 && !is_last;
            let mut continues_path = ancestor_continues.to_vec();
            continues_path.push(continues);
            walk_tree(
                out,
                children_map,
                child,
                depth + 1,
                &continues_path,
                child_is_last,
            );
        }
    }
}

/// Delete a session file, trying the `trash` CLI first, then falling back to
/// permanent deletion (`deleteSessionFile`, `session-selector.ts:539-576`).
/// Returns "trash" or "unlink" on success.
fn delete_session_file(path: &Path) -> Result<&'static str, String> {
    let mut command = std::process::Command::new("trash");
    // Guard against paths that would parse as trash flags.
    if path.to_string_lossy().starts_with('-') {
        command.arg("--");
    }
    command.arg(path);
    let attempted = command.output().ok();
    let stderr = attempted
        .as_ref()
        .map(|output| String::from_utf8_lossy(&output.stderr).trim().to_string());
    let succeeded = attempted.is_some_and(|output| output.status.success());
    if succeeded || !path.exists() {
        return Ok("trash");
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok("unlink"),
        Err(error) => {
            let mut hint = String::new();
            if let Some(stderr) = stderr.as_deref().filter(|stderr| !stderr.is_empty()) {
                let first_line = stderr.lines().next().unwrap_or(stderr);
                hint = format!(" (trash: {first_line})");
            }
            Err(format!("{error}{hint}"))
        }
    }
}

/// A status line for the header (`setStatusMessage`); the port has no timer,
/// so messages persist until the next state change (documented deviation).
#[derive(Debug, Clone, PartialEq)]
enum StatusKind {
    Info,
    Error,
}

/// The header strip (`SessionSelectorHeader`, `session-selector.ts:35-187`):
/// title left, scope/name/sort state right, two hint lines beneath that
/// reflect the dialog's transient states.
struct SessionSelectorHeader {
    scope_all: bool,
    sort_mode: SortMode,
    name_filter: NameFilter,
    show_path: bool,
    confirming_delete_path: Option<PathBuf>,
    status: Option<(StatusKind, String)>,
    show_rename_hint: bool,
}

impl SessionSelectorHeader {
    fn key_text(action_id: &str) -> String {
        // The full registry: tui.* actions live in pi-tui, app.* in the mode.
        pi_tui::keybindings::keys_for(action_id)
            .first()
            .or_else(|| theme::app_keys::keys_for(action_id).first())
            .map_or_else(String::new, |key| (*key).to_string())
    }

    fn key_hint(action_id: &str, description: &str) -> String {
        let theme = theme::theme();
        format!(
            "{} {}",
            theme.fg(ThemeColor::Dim, &Self::key_text(action_id)),
            theme.fg(ThemeColor::Muted, description)
        )
    }

    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let title = if self.scope_all {
            "Resume Session (All)"
        } else {
            "Resume Session (Current Folder)"
        };
        let left_text = theme::bold(title);
        let sort_label = match self.sort_mode {
            SortMode::Threaded => "Threaded",
            SortMode::Recent => "Recent",
            SortMode::Relevance => "Fuzzy",
        };
        let sort_text = format!(
            "{}{}",
            theme.fg(ThemeColor::Muted, "Sort: "),
            theme.fg(ThemeColor::Accent, sort_label)
        );
        let name_label = if self.name_filter == NameFilter::All {
            "All"
        } else {
            "Named"
        };
        let name_text = format!(
            "{}{}",
            theme.fg(ThemeColor::Muted, "Name: "),
            theme.fg(ThemeColor::Accent, name_label)
        );
        let (current, all) = if self.scope_all {
            (
                theme.fg(ThemeColor::Muted, "○ Current Folder"),
                theme.fg(ThemeColor::Accent, "◉ All"),
            )
        } else {
            (
                theme.fg(ThemeColor::Accent, "◉ Current Folder"),
                theme.fg(ThemeColor::Muted, "○ All"),
            )
        };
        let scope_text = format!("{current} | {all}");
        let right_text = truncate_to_width(
            &format!("{scope_text}  {name_text}  {sort_text}"),
            width,
            "",
            false,
        );
        let available_left = width.saturating_sub(visible_width(&right_text) + 1);
        let left = truncate_to_width(&left_text, available_left, "", false);
        let spacing = width.saturating_sub(visible_width(&left) + visible_width(&right_text));
        let (hint_line1, hint_line2) = self.render_hints(width);
        vec![
            format!("{left}{}{right_text}", " ".repeat(spacing)),
            hint_line1,
            hint_line2,
        ]
    }

    /// The two hint lines, by state: delete confirmation, status message,
    /// then the normal two-liner (`render`'s hint block,
    /// `session-selector.ts:119-148`).
    fn render_hints(&self, width: usize) -> (String, String) {
        let theme = theme::theme();
        if self.confirming_delete_path.is_some() {
            let confirm_hint = format!(
                "Delete session? {} · {}",
                Self::key_hint(action::SELECT_CONFIRM, "confirm"),
                Self::key_hint(action::SELECT_CANCEL, "cancel")
            );
            return (
                theme.fg(
                    ThemeColor::Error,
                    &truncate_to_width(&confirm_hint, width, "…", false),
                ),
                String::new(),
            );
        }
        if let Some((kind, message)) = &self.status {
            let color = if *kind == StatusKind::Error {
                ThemeColor::Error
            } else {
                ThemeColor::Accent
            };
            return (
                theme.fg(color, &truncate_to_width(message, width, "…", false)),
                String::new(),
            );
        }
        let path_state = if self.show_path { "(on)" } else { "(off)" };
        let sep = theme.fg(ThemeColor::Muted, " · ");
        let hint1 = format!(
            "{}{sep}{}",
            Self::key_hint(action::INPUT_TAB, "scope"),
            theme.fg(ThemeColor::Muted, "re:<pattern> regex · \"phrase\" exact")
        );
        let mut hint2_parts = vec![
            Self::key_hint(theme::app_keys::SESSION_TOGGLE_SORT, "sort"),
            Self::key_hint(theme::app_keys::SESSION_TOGGLE_NAMED_FILTER, "named"),
            Self::key_hint(theme::app_keys::SESSION_DELETE, "delete"),
            Self::key_hint(
                theme::app_keys::SESSION_TOGGLE_PATH,
                &format!("path {path_state}"),
            ),
        ];
        if self.show_rename_hint {
            hint2_parts.push(Self::key_hint(theme::app_keys::SESSION_RENAME, "rename"));
        }
        let hint2 = hint2_parts.join(&sep);
        (
            truncate_to_width(&hint1, width, "…", false),
            truncate_to_width(&hint2, width, "…", false),
        )
    }
}

/// The session list (`SessionList`, `session-selector.ts:217-537`): search
/// input over a scrolling window of one-line rows, fork-tree prefixes, and
/// the delete-confirmation state.
struct SessionList {
    all_sessions: Vec<SessionInfo>,
    filtered: Vec<SessionNode>,
    selected_index: usize,
    selection_touched: bool,
    search_input: Input,
    show_cwd: bool,
    sort_mode: SortMode,
    name_filter: NameFilter,
    show_path: bool,
    confirming_delete_path: Option<PathBuf>,
    current_session_path_key: Option<PathBuf>,
    max_visible: usize,
}

impl SessionList {
    fn new(
        all_sessions: Vec<SessionInfo>,
        show_cwd: bool,
        sort_mode: SortMode,
        name_filter: NameFilter,
        current_session_file: Option<&Path>,
    ) -> Self {
        let mut list = Self {
            all_sessions,
            filtered: Vec::new(),
            selected_index: 0,
            selection_touched: false,
            search_input: Input::new(InputOptions::default()),
            show_cwd,
            sort_mode,
            name_filter,
            show_path: false,
            confirming_delete_path: None,
            current_session_path_key: current_session_file.map(session_path_key),
            max_visible: 10,
        };
        list.filter_sessions("");
        list
    }

    fn selected_session_path(&self) -> Option<PathBuf> {
        self.filtered
            .get(self.selected_index)
            .map(|node| self.all_sessions[node.session_index].path.clone())
    }

    fn is_current_session_path(&self, path: &Path) -> bool {
        self.current_session_path_key
            .as_ref()
            .is_some_and(|key| session_path_key(path) == *key)
    }

    /// `setSessions` (`session-selector.ts:281-295`): keep the selection on
    /// the same session when the user has moved the cursor.
    fn set_sessions(&mut self, sessions: Vec<SessionInfo>, show_cwd: bool) {
        let selected_path = self
            .selection_touched
            .then(|| self.selected_session_path())
            .flatten();
        self.all_sessions = sessions;
        self.show_cwd = show_cwd;
        let query = self.search_input.value().to_string();
        self.filter_sessions(&query);
        if !self.selection_touched {
            self.selected_index = 0;
        } else if let Some(selected_path) = selected_path
            && let Some(index) = self
                .filtered
                .iter()
                .position(|node| self.all_sessions[node.session_index].path == selected_path)
        {
            self.selected_index = index;
        }
    }

    /// `filterSessions` (`session-selector.ts:297-322`): the name filter
    /// applies first; threaded mode without a query shows the fork tree,
    /// everything else is a flat scored list.
    fn filter_sessions(&mut self, query: &str) {
        let name_filtered: Vec<usize> = (0..self.all_sessions.len())
            .filter(|&index| match self.name_filter {
                NameFilter::All => true,
                NameFilter::Named => has_session_name(&self.all_sessions[index]),
            })
            .collect();
        let trimmed = query.trim();
        if self.sort_mode == SortMode::Threaded && trimmed.is_empty() {
            self.filtered = flatten_session_tree(&self.all_sessions, &name_filtered);
        } else {
            let subset: Vec<SessionInfo> = name_filtered
                .iter()
                .map(|&index| self.all_sessions[index].clone())
                .collect();
            let results = filter_and_sort_sessions(&subset, query, self.sort_mode, NameFilter::All);
            let index_of: HashMap<&Path, usize> = self
                .all_sessions
                .iter()
                .enumerate()
                .map(|(index, session)| (session.path.as_path(), index))
                .collect();
            self.filtered = results
                .iter()
                .filter_map(|session| {
                    index_of
                        .get(session.path.as_path())
                        .map(|&session_index| SessionNode {
                            session_index,
                            depth: 0,
                            is_last: true,
                            ancestor_continues: Vec::new(),
                        })
                })
                .collect();
        }
        self.selected_index = self
            .selected_index
            .min(self.filtered.len().saturating_sub(1));
    }

    fn start_delete_confirmation(&mut self) -> Option<String> {
        let selected = self.filtered.get(self.selected_index)?;
        let path = self.all_sessions[selected.session_index].path.clone();
        // Prevent deleting the currently active session.
        if self.is_current_session_path(&path) {
            return Some("Cannot delete the currently active session".to_string());
        }
        self.confirming_delete_path = Some(path);
        None
    }

    /// `buildTreePrefix` (`session-selector.ts:451-460`).
    fn build_tree_prefix(node: &SessionNode) -> String {
        if node.depth == 0 {
            return String::new();
        }
        let mut prefix = String::new();
        for continues in &node.ancestor_continues {
            prefix.push_str(if *continues { "│  " } else { "   " });
        }
        prefix.push_str(if node.is_last { "└─ " } else { "├─ " });
        prefix
    }

    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let mut lines = Vec::new();
        lines.extend(self.search_input.render(width));
        lines.push(String::new());
        if self.filtered.is_empty() {
            let empty_message = if self.name_filter == NameFilter::Named {
                let toggle_key = Self::key_text_of(theme::app_keys::SESSION_TOGGLE_NAMED_FILTER);
                if self.show_cwd {
                    format!("  No named sessions found. Press {toggle_key} to show all.")
                } else {
                    format!(
                        "  No named sessions in current folder. Press {toggle_key} to show all, or Tab to view all."
                    )
                }
            } else if self.show_cwd {
                "  No sessions found".to_string()
            } else {
                "  No sessions in current folder. Press Tab to view all.".to_string()
            };
            lines.push(theme.fg(
                ThemeColor::Muted,
                &truncate_to_width(&empty_message, width, "…", false),
            ));
            return lines;
        }
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible / 2)
            .min(self.filtered.len().saturating_sub(self.max_visible));
        let end_index = (start_index + self.max_visible).min(self.filtered.len());
        for i in start_index..end_index {
            lines.push(self.render_row(i, i == self.selected_index, width));
        }
        if start_index > 0 || end_index < self.filtered.len() {
            let scroll_text = format!("  ({}/{})", self.selected_index + 1, self.filtered.len());
            lines.push(theme.fg(
                ThemeColor::Muted,
                &truncate_to_width(&scroll_text, width, "", false),
            ));
        }
        lines
    }

    /// One session row (`render`'s per-item block,
    /// `session-selector.ts:371-420`): tree prefix, name-or-first-message,
    /// right side with age/message-count/cwd/path, and the selection state.
    fn render_row(&self, index: usize, is_selected: bool, width: usize) -> String {
        let theme = theme::theme();
        let node = &self.filtered[index];
        let session = &self.all_sessions[node.session_index];
        let is_confirming_delete = self
            .confirming_delete_path
            .as_ref()
            .is_some_and(|path| *path == session.path);
        let is_current = self.is_current_session_path(&session.path);
        let prefix = Self::build_tree_prefix(node);
        let has_name = session.name.is_some();
        let display_text = session
            .name
            .clone()
            .unwrap_or_else(|| session.first_message.clone());
        let normalized_message: String = display_text
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>()
            .trim()
            .to_string();
        let age = format_session_date(session.modified);
        let mut right_part = format!("{} {age}", session.message_count);
        if self.show_cwd && !session.cwd.is_empty() {
            right_part = format!(
                "{} {right_part}",
                crate::tools::render_utils::shorten_path(&session.cwd)
            );
        }
        if self.show_path {
            right_part = format!(
                "{} {right_part}",
                crate::tools::render_utils::shorten_path(&session.path.to_string_lossy())
            );
        }
        let cursor = if is_selected {
            theme.fg(ThemeColor::Accent, "› ")
        } else {
            "  ".to_string()
        };
        let prefix_width = visible_width(&prefix);
        let right_width = visible_width(&right_part) + 2;
        let available_for_msg = width.saturating_sub(2 + prefix_width + right_width);
        let truncated_msg =
            truncate_to_width(&normalized_message, available_for_msg.max(10), "…", false);
        let message_color = if is_confirming_delete {
            Some(ThemeColor::Error)
        } else if is_current {
            Some(ThemeColor::Accent)
        } else if has_name {
            Some(ThemeColor::Warning)
        } else {
            None
        };
        let mut styled_msg = match message_color {
            Some(color) => theme.fg(color, &truncated_msg),
            None => truncated_msg,
        };
        if is_selected {
            styled_msg = theme::bold(&styled_msg);
        }
        let left_part = format!("{cursor}{}{styled_msg}", theme.fg(ThemeColor::Dim, &prefix));
        let spacing =
            (width.saturating_sub(visible_width(&left_part) + visible_width(&right_part))).max(1);
        let styled_right = theme.fg(
            if is_confirming_delete {
                ThemeColor::Error
            } else {
                ThemeColor::Dim
            },
            &right_part,
        );
        let mut line = format!("{left_part}{}{styled_right}", " ".repeat(spacing));
        if is_selected {
            line = theme.bg(ThemeBg::SelectedBg, &line);
        }
        truncate_to_width(&line, width, "", false)
    }

    fn key_text_of(action_id: &str) -> String {
        SessionSelectorHeader::key_text(action_id)
    }

    /// `handleInput` (`session-selector.ts:440-536`). The delete/rename/sort
    /// actions return small intents for the component, which owns the caches
    /// and the header.
    fn handle_key(&mut self, data: &str) -> ListAction {
        // Delete confirmation intercepts all keys first.
        if self.confirming_delete_path.is_some() {
            if keybindings::matches(data, action::SELECT_CONFIRM) {
                let confirmed = self.confirming_delete_path.take();
                return ListAction::DeleteConfirmed(confirmed);
            }
            if keybindings::matches(data, action::SELECT_CANCEL) {
                self.confirming_delete_path = None;
            }
            return ListAction::None;
        }
        if keybindings::matches(data, action::INPUT_TAB) {
            return ListAction::ToggleScope;
        }
        if theme::app_keys::matches(data, theme::app_keys::SESSION_TOGGLE_SORT) {
            return ListAction::ToggleSort;
        }
        if theme::app_keys::matches(data, theme::app_keys::SESSION_TOGGLE_NAMED_FILTER) {
            return ListAction::ToggleNameFilter;
        }
        if theme::app_keys::matches(data, theme::app_keys::SESSION_TOGGLE_PATH) {
            self.show_path = !self.show_path;
            return ListAction::ToggledPath(self.show_path);
        }
        if theme::app_keys::matches(data, theme::app_keys::SESSION_DELETE) {
            if let Some(error) = self.start_delete_confirmation() {
                return ListAction::Error(error);
            }
            return ListAction::None;
        }
        if theme::app_keys::matches(data, theme::app_keys::SESSION_RENAME) {
            if let Some(selected) = self.filtered.get(self.selected_index) {
                let path = self.all_sessions[selected.session_index].path.clone();
                return ListAction::Rename(path);
            }
            return ListAction::None;
        }
        // Ctrl+backspace: delete when the query is empty, else it edits the
        // query like a plain word-delete.
        if theme::app_keys::matches(data, theme::app_keys::SESSION_DELETE_NONINVASIVE) {
            if !self.search_input.value().is_empty() {
                self.search_input.handle_key(data);
                let query = self.search_input.value().to_string();
                self.filter_sessions(&query);
                return ListAction::None;
            }
            if let Some(error) = self.start_delete_confirmation() {
                return ListAction::Error(error);
            }
            return ListAction::None;
        }
        self.selection_touched = true;
        if keybindings::matches(data, action::SELECT_UP) {
            self.selected_index = self.selected_index.saturating_sub(1);
        } else if keybindings::matches(data, action::SELECT_DOWN) {
            self.selected_index =
                (self.selected_index + 1).min(self.filtered.len().saturating_sub(1));
        } else if keybindings::matches(data, action::SELECT_PAGE_UP) {
            self.selected_index = self.selected_index.saturating_sub(self.max_visible);
        } else if keybindings::matches(data, action::SELECT_PAGE_DOWN) {
            self.selected_index =
                (self.selected_index + self.max_visible).min(self.filtered.len().saturating_sub(1));
        } else if keybindings::matches(data, action::SELECT_CONFIRM) {
            if let Some(path) = self.selected_session_path() {
                return ListAction::Select(path);
            }
        } else if keybindings::matches(data, action::SELECT_CANCEL) {
            return ListAction::Cancel;
        } else {
            self.search_input.handle_key(data);
            let query = self.search_input.value().to_string();
            self.filter_sessions(&query);
        }
        ListAction::None
    }
}

/// Intents `SessionList::handle_key` hands back to the component.
enum ListAction {
    None,
    Select(PathBuf),
    Cancel,
    ToggleScope,
    ToggleSort,
    ToggleNameFilter,
    ToggledPath(bool),
    DeleteConfirmed(Option<PathBuf>),
    Rename(PathBuf),
    Error(String),
}

/// The dialog (`SessionSelectorComponent`, `session-selector.ts:577-884`).
pub struct SessionSelectorComponent {
    cwd: PathBuf,
    session_dir: Option<PathBuf>,
    uses_default_session_dir: bool,
    current_sessions: Vec<SessionInfo>,
    all_sessions: Option<Vec<SessionInfo>>,
    scope_all: bool,
    sort_mode: SortMode,
    name_filter: NameFilter,
    header: SessionSelectorHeader,
    session_list: SessionList,
    mode: ViewMode,
    rename_input: Input,
    rename_target_path: Option<PathBuf>,
    events: Arc<Mutex<Vec<SessionSelectorEvent>>>,
}

enum ViewMode {
    List,
    Rename,
}

impl SessionSelectorComponent {
    /// The constructor (`session-selector.ts:636-676`): current-folder
    /// sessions load immediately.
    pub fn new(
        cwd: &Path,
        session_dir: Option<&Path>,
        uses_default_session_dir: bool,
        current_session_file: Option<&Path>,
        events: Arc<Mutex<Vec<SessionSelectorEvent>>>,
    ) -> Self {
        let current_sessions = SessionManager::list(cwd, session_dir);
        let sort_mode = SortMode::Threaded;
        let name_filter = NameFilter::All;
        let header = SessionSelectorHeader {
            scope_all: false,
            sort_mode,
            name_filter,
            show_path: false,
            confirming_delete_path: None,
            status: None,
            show_rename_hint: true,
        };
        let session_list = SessionList::new(
            current_sessions.clone(),
            false,
            sort_mode,
            name_filter,
            current_session_file,
        );
        Self {
            cwd: cwd.to_path_buf(),
            session_dir: session_dir.map(Path::to_path_buf),
            uses_default_session_dir,
            current_sessions,
            all_sessions: None,
            scope_all: false,
            sort_mode,
            name_filter,
            header,
            session_list,
            mode: ViewMode::List,
            rename_input: Input::new(InputOptions::default()),
            rename_target_path: None,
            events,
        }
    }

    fn load_all(&self) -> Vec<SessionInfo> {
        if self.uses_default_session_dir {
            SessionManager::list_all(None)
        } else {
            SessionManager::list_all(self.session_dir.as_deref())
        }
    }

    fn set_status(&mut self, kind: StatusKind, message: impl Into<String>) {
        self.header.status = Some((kind, message.into()));
    }

    /// `buildBaseLayout` (`session-selector.ts:628-635`).
    fn render_base(&self, content: Vec<String>, show_header: bool, width: usize) -> Vec<String> {
        let border = DynamicBorder::new();
        let mut lines = vec![String::new()];
        lines.extend(border.render(width));
        lines.push(String::new());
        if show_header {
            lines.extend(self.header.render(width));
            lines.push(String::new());
        }
        lines.extend(content);
        lines.push(String::new());
        lines.extend(border.render(width));
        lines
    }

    /// `loadScope` (`session-selector.ts:777-851`), synchronous.
    fn load_scope(&mut self, scope_all: bool) {
        if scope_all {
            if self.all_sessions.is_none() {
                self.all_sessions = Some(self.load_all());
            }
        } else {
            self.current_sessions = SessionManager::list(&self.cwd, self.session_dir.as_deref());
        }
        let sessions = self.sessions_for_scope(scope_all);
        self.session_list.set_sessions(sessions, scope_all);
    }

    fn sessions_for_scope(&self, scope_all: bool) -> Vec<SessionInfo> {
        if scope_all {
            self.all_sessions.clone().unwrap_or_default()
        } else {
            self.current_sessions.clone()
        }
    }

    /// `refreshSessionsAfterMutation` (`session-selector.ts:860-864`).
    fn refresh_after_mutation(&mut self) {
        self.load_scope(self.scope_all);
    }

    fn toggle_sort_mode(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Threaded => SortMode::Recent,
            SortMode::Recent => SortMode::Relevance,
            SortMode::Relevance => SortMode::Threaded,
        };
        self.header.sort_mode = self.sort_mode;
        self.session_list.sort_mode = self.sort_mode;
        let query = self.session_list.search_input.value().to_string();
        self.session_list.filter_sessions(&query);
    }

    fn toggle_name_filter(&mut self) {
        self.name_filter = if self.name_filter == NameFilter::All {
            NameFilter::Named
        } else {
            NameFilter::All
        };
        self.header.name_filter = self.name_filter;
        self.session_list.name_filter = self.name_filter;
        let query = self.session_list.search_input.value().to_string();
        self.session_list.filter_sessions(&query);
    }

    fn toggle_scope(&mut self) {
        self.scope_all = !self.scope_all;
        self.header.scope_all = self.scope_all;
        let sessions = self.sessions_for_scope(self.scope_all);
        self.session_list.set_sessions(sessions, self.scope_all);
        if self.scope_all && self.all_sessions.is_none() {
            self.load_scope(true);
        }
    }

    /// `enterRenameMode` (`session-selector.ts:730-748`).
    fn enter_rename_mode(&mut self, session_path: PathBuf) {
        let sessions = self.sessions_for_scope(self.scope_all);
        let current_name = sessions
            .iter()
            .find(|session| session.path == session_path)
            .and_then(|session| session.name.clone());
        self.mode = ViewMode::Rename;
        self.rename_target_path = Some(session_path);
        self.rename_input
            .set_value(current_name.unwrap_or_default());
        Focusable::set_focused(&mut self.rename_input, true);
    }

    /// `exitRenameMode` (`session-selector.ts:749-755`).
    fn exit_rename_mode(&mut self) {
        self.mode = ViewMode::List;
        self.rename_target_path = None;
        Focusable::set_focused(&mut self.rename_input, false);
    }

    /// `confirmRename` (`session-selector.ts:757-775`): an empty name keeps
    /// the mode; a name persists via `appendSessionInfo`.
    fn confirm_rename(&mut self, value: &str) {
        let next = value.trim();
        if next.is_empty() {
            return;
        }
        let Some(target) = self.rename_target_path.clone() else {
            self.exit_rename_mode();
            return;
        };
        let mut manager = match SessionManager::open(&target, None, None) {
            Ok(manager) => manager,
            Err(error) => {
                self.set_status(StatusKind::Error, format!("Failed to rename: {error}"));
                return;
            }
        };
        if let Err(error) = manager.append_session_info(next) {
            self.set_status(StatusKind::Error, format!("Failed to rename: {error}"));
            return;
        }
        self.refresh_after_mutation();
        self.exit_rename_mode();
    }

    /// `onDeleteSession` (`session-selector.ts:690-715`), synchronous.
    fn delete_confirmed(&mut self, path: Option<PathBuf>) {
        let Some(path) = path else {
            return;
        };
        match delete_session_file(&path) {
            Ok(method) => {
                self.current_sessions.retain(|session| session.path != path);
                if let Some(all) = &mut self.all_sessions {
                    all.retain(|session| session.path != path);
                }
                let sessions = self.sessions_for_scope(self.scope_all);
                self.session_list.set_sessions(sessions, self.scope_all);
                let message = if method == "trash" {
                    "Session moved to trash"
                } else {
                    "Session deleted"
                };
                self.set_status(StatusKind::Info, message);
                self.refresh_after_mutation();
            }
            Err(error) => {
                self.set_status(StatusKind::Error, format!("Failed to delete: {error}"));
            }
        }
    }

    fn handle_list_key(&mut self, data: &str) {
        let action = self.session_list.handle_key(data);
        // `onDeleteConfirmationChange` (`session-selector.ts:683-686`): the
        // header mirrors the list's delete-confirmation state.
        self.header.confirming_delete_path = self.session_list.confirming_delete_path.clone();
        match action {
            ListAction::None => {}
            ListAction::Select(path) => {
                self.header.status = None;
                self.events
                    .lock()
                    .expect("session selector events")
                    .push(SessionSelectorEvent::Selected(path));
            }
            ListAction::Cancel => {
                self.header.status = None;
                self.events
                    .lock()
                    .expect("session selector events")
                    .push(SessionSelectorEvent::Cancel);
            }
            ListAction::ToggleScope => self.toggle_scope(),
            ListAction::ToggleSort => self.toggle_sort_mode(),
            ListAction::ToggleNameFilter => self.toggle_name_filter(),
            ListAction::ToggledPath(show_path) => {
                self.header.show_path = show_path;
            }
            ListAction::DeleteConfirmed(path) => self.delete_confirmed(path),
            ListAction::Rename(path) => self.enter_rename_mode(path),
            ListAction::Error(message) => self.set_status(StatusKind::Error, message),
        }
    }
}

impl Component for SessionSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        match &self.mode {
            ViewMode::List => self.render_base(self.session_list.render(width), true, width),
            ViewMode::Rename => {
                let theme = theme::theme();
                let mut panel = vec![theme::bold("Rename Session"), String::new()];
                panel.extend(self.rename_input.render(width));
                panel.push(String::new());
                panel.push(theme.fg(
                    ThemeColor::Muted,
                    &format!(
                        "{} to save · {} to cancel",
                        SessionSelectorHeader::key_text(action::SELECT_CONFIRM),
                        SessionSelectorHeader::key_text(action::SELECT_CANCEL)
                    ),
                ));
                self.render_base(panel, false, width)
            }
        }
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        if matches!(self.mode, ViewMode::Rename) {
            if keybindings::matches(data, action::SELECT_CANCEL) {
                self.exit_rename_mode();
                return Some(ComponentMutation { consume: true });
            }
            if let InputAction::Submitted(value) = self.rename_input.handle_key(data) {
                self.confirm_rename(&value);
            }
            return Some(ComponentMutation { consume: true });
        }
        self.handle_list_key(data);
        Some(ComponentMutation { consume: true })
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
#[cfg_attr(test, allow(clippy::assert_is_empty))]
mod tests {
    use super::*;
    use crate::session::store::NewSessionOptions;
    use pi_tui::ansi::strip_terminal_sequences;
    use pi_tui::terminal::{CaptureTerminal, TerminalSize};

    fn fixture_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir");
        (dir, sessions)
    }

    fn make_session(cwd: &Path, session_dir: &Path, first_message: &str) -> PathBuf {
        let mut session =
            SessionManager::create(cwd, Some(session_dir), &NewSessionOptions::default())
                .expect("create session");
        session
            .append_message(pi_agent::AgentMessage::User(pi_ai::types::UserMessage {
                content: pi_ai::types::UserContent::Text(first_message.to_string()),
                timestamp: 0,
            }))
            .expect("append message");
        session.session_file().expect("session file").to_path_buf()
    }

    fn fixture_component(
        cwd: &Path,
        session_dir: &Path,
        current: Option<&Path>,
    ) -> (
        SessionSelectorComponent,
        Arc<Mutex<Vec<SessionSelectorEvent>>>,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let component = SessionSelectorComponent::new(
            cwd,
            Some(session_dir),
            false,
            current,
            Arc::clone(&events),
        );
        (component, events)
    }

    fn rendered(component: &SessionSelectorComponent) -> String {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 100,
            rows: 40,
        });
        let lines = Component::render(component, 100);
        let _ = terminal;
        strip_terminal_sequences(&lines.join("\n"))
    }

    #[test]
    fn empty_current_scope_shows_the_all_hint() {
        let (dir, sessions) = fixture_dir();
        let (component, _events) = fixture_component(dir.path(), &sessions, None);
        let plain = rendered(&component);
        assert!(
            plain.contains("No sessions in current folder. Press Tab to view all."),
            "{plain}"
        );
        assert!(plain.contains("Resume Session (Current Folder)"), "{plain}");
    }

    #[test]
    fn enter_selects_the_highlighted_session() {
        let (dir, sessions) = fixture_dir();
        let path = make_session(dir.path(), &sessions, "the first prompt");
        let (mut component, events) = fixture_component(dir.path(), &sessions, None);
        let plain = rendered(&component);
        assert!(plain.contains("the first prompt"), "{plain}");
        component.handle_input("\r");
        assert_eq!(
            *events.lock().expect("events"),
            vec![SessionSelectorEvent::Selected(path)]
        );
    }

    #[test]
    fn a_query_filters_before_enter_selects() {
        let (dir, sessions) = fixture_dir();
        make_session(dir.path(), &sessions, "the alpha prompt");
        let beta = make_session(dir.path(), &sessions, "the beta needle prompt");
        let (mut component, events) = fixture_component(dir.path(), &sessions, None);
        for character in "needle".chars() {
            component.handle_input(&character.to_string());
        }
        let plain = rendered(&component);
        assert!(plain.contains("beta needle"), "{plain}");
        assert!(!plain.contains("alpha prompt"), "{plain}");
        component.handle_input("\r");
        assert_eq!(
            *events.lock().expect("events"),
            vec![SessionSelectorEvent::Selected(beta)]
        );
    }

    #[test]
    fn escape_cancels() {
        let (dir, sessions) = fixture_dir();
        make_session(dir.path(), &sessions, "prompt");
        let (mut component, events) = fixture_component(dir.path(), &sessions, None);
        component.handle_input("\x1b");
        assert_eq!(
            *events.lock().expect("events"),
            vec![SessionSelectorEvent::Cancel]
        );
    }

    #[test]
    fn tab_switches_to_the_all_scope() {
        let (dir, sessions) = fixture_dir();
        // A session under a different cwd: visible only in the all scope.
        let other_cwd = dir.path().join("other");
        std::fs::create_dir_all(&other_cwd).expect("mkdir");
        make_session(&other_cwd, &sessions, "the foreign prompt");
        let (mut component, _events) = fixture_component(dir.path(), &sessions, None);
        assert!(!rendered(&component).contains("the foreign prompt"));
        component.handle_input("\t");
        let plain = rendered(&component);
        assert!(plain.contains("Resume Session (All)"), "{plain}");
        assert!(plain.contains("the foreign prompt"), "{plain}");
    }

    #[test]
    fn delete_confirm_removes_the_file_and_skips_the_current_session() {
        let (dir, sessions) = fixture_dir();
        let current = make_session(dir.path(), &sessions, "current session");
        let other = make_session(dir.path(), &sessions, "other session");
        let (mut component, _events) = fixture_component(dir.path(), &sessions, Some(&current));

        // The current session refuses deletion (select it by query so the
        // modified-descending list order doesn't matter).
        for character in "current".chars() {
            component.handle_input(&character.to_string());
        }
        component.handle_input("\x04");
        let plain = rendered(&component);
        assert!(
            plain.contains("Cannot delete the currently active session"),
            "{plain}"
        );
        assert!(current.exists());

        // A fresh dialog: the other session asks, then deletes on confirm.
        let (mut component, _events) = fixture_component(dir.path(), &sessions, Some(&current));
        for character in "other".chars() {
            component.handle_input(&character.to_string());
        }
        component.handle_input("\x04");
        let plain = rendered(&component);
        assert!(plain.contains("Delete session?"), "{plain}");
        component.handle_input("\r");
        assert!(!other.exists(), "session file should be deleted");
        assert!(current.exists());
    }

    #[test]
    fn delete_cancel_keeps_the_file() {
        let (dir, sessions) = fixture_dir();
        let current = make_session(dir.path(), &sessions, "current session");
        let other = make_session(dir.path(), &sessions, "other session");
        let (mut component, _events) = fixture_component(dir.path(), &sessions, Some(&current));
        for character in "other".chars() {
            component.handle_input(&character.to_string());
        }
        component.handle_input("\x04");
        component.handle_input("\x1b");
        assert!(other.exists());
    }

    #[test]
    fn rename_persists_a_session_name() {
        let (dir, sessions) = fixture_dir();
        let current = make_session(dir.path(), &sessions, "current session");
        let target = make_session(dir.path(), &sessions, "the renamable prompt");
        let (mut component, _events) = fixture_component(dir.path(), &sessions, Some(&current));
        for character in "renamable".chars() {
            component.handle_input(&character.to_string());
        }
        component.handle_input("\x12");
        let plain = rendered(&component);
        assert!(plain.contains("Rename Session"), "{plain}");
        for character in "my name".chars() {
            component.handle_input(&character.to_string());
        }
        component.handle_input("\r");
        let reopened = SessionManager::open(&target, None, None).expect("reopen");
        assert_eq!(reopened.get_session_name().as_deref(), Some("my name"));
        // The list refreshes and shows the new name.
        let plain = rendered(&component);
        assert!(plain.contains("my name"), "{plain}");
    }
}
