//! `TreeSelectorComponent` — the `/tree` dialog.
//!
//! Ground truth: `modes/interactive/components/tree-selector.ts` (1-1439).
//! The tree list flattens the session tree with pi's indentation rules
//! (`flattenTree`, `:203-331`), filters it (`applyFilter`, `:333-431`) across
//! the five display modes (`FilterMode`, `:98`), recomputes the visual
//! structure for the filtered view (`recalculateVisualStructure`, `:439-562`),
//! renders a horizontally panning viewport that keeps the gutter visible
//! (`renderHorizontalViewport`, `:65-95`), and handles the fold/unfold,
//! filter, search, label, copy, and page keys (`handleInput`, `:995-1101`).
//!
//! Deviations: upstream composes `TreeList` + `SearchLine` + `LabelInput` as
//! pi-tui container children with a `setTimeout(onCancel, 100)` auto-cancel
//! for an empty tree; the port renders the same pieces inside one component,
//! and the orchestrator refuses an empty tree before mounting
//! (`showTreeSelector`'s guard, `interactive-mode.ts:5756-5759`), so the
//! auto-cancel has no ported counterpart. Unparseable label timestamps render
//! as no timestamp (upstream's `Invalid Date` would print `NaN:NaN`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Datelike, Local, Utc};
use pi_tui::ansi::{
    CURSOR_MARKER, slice_by_column, truncate_to_width, visible_width, wrap_text_with_ansi,
};
use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::input::{Input, InputOptions};
use pi_tui::components::text::Text;
use pi_tui::keybindings::{self, action};
use pi_tui::theme::{ThemeBg, ThemeColor};

use crate::config::settings::TreeFilterMode;
use crate::session::records::SessionEntry;
use crate::session::store::{SessionTreeNode, iso_to_ms, now_iso8601};
use crate::tools::render_utils::{format_read_line_range, shorten_home_path};

use super::super::theme;
use super::header::DynamicBorder;

/// Events the selector publishes to the orchestrator — the four upstream
/// callbacks (`onSelect`/`onCancel`/`onCopy`/`onLabelChange`).
#[derive(Debug, Clone, PartialEq)]
pub enum TreeSelectorEvent {
    /// `onSelect(entryId)` — the highlighted entry was confirmed.
    Selected(String),
    /// `onCancel` — escape with an empty search.
    Cancel,
    /// `onCopy(text)` — ctrl+x on the selected entry; `None` when the entry
    /// has no copyable text (`getEntryCopyText`, `tree-selector.ts:907-934`).
    Copy(Option<String>),
    /// `onLabelChange(entryId, label)` — the label input committed; `None`
    /// clears the label.
    LabelChange(String, Option<String>),
}

/// What one `TreeList.handle_input` round produced — the component maps these
/// onto the event queue, except `LabelEdit`, which it handles internally like
/// upstream's `TreeList.onLabelEdit` → `showLabelInput`
/// (`tree-selector.ts:1374`, `:1083-1087`).
enum TreeListOutcome {
    None,
    Selected(String),
    Cancel,
    Copy(Option<String>),
    LabelEdit(String, Option<String>),
}

/// Gutter info: position (display indent where the connector was) and whether
/// to show │ (`GutterInfo`, `tree-selector.ts:24-27`).
#[derive(Debug, Clone, Copy)]
struct GutterInfo {
    position: usize,
    show: bool,
}

/// Flattened tree node for navigation (`FlatNode`, `tree-selector.ts:30-42`).
struct FlatNode {
    node: SessionTreeNode,
    /// Indentation level (each level = 3 chars).
    indent: usize,
    /// Whether to show a connector (├─ or └─) — true if the parent has
    /// multiple children.
    show_connector: bool,
    /// If showing a connector, true = last sibling (└─).
    is_last: bool,
    /// Gutter info for each ancestor branch point.
    gutters: Vec<GutterInfo>,
    /// True if this node is a root under a virtual branching root.
    is_virtual_root_child: bool,
}

/// A row for the horizontal viewport
/// (`HorizontalViewportRow`, `tree-selector.ts:44-50`).
struct ViewportRow {
    gutter: String,
    body: String,
    anchor_col: usize,
    body_width: usize,
    is_selected: bool,
}

const TREE_GUTTER_WIDTH: usize = 2;
const MIN_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 4;
const MAX_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 20;
const MIN_ANCHOR_CONTEXT_WIDTH: usize = 2;
const MAX_ANCHOR_CONTEXT_WIDTH: usize = 12;

/// Render tree rows into a horizontally clipped viewport
/// (`renderHorizontalViewport`, `tree-selector.ts:65-95`). The tree gutter
/// stays visible; the bodies shift left only when the selected row's anchor
/// would otherwise be too far right to see useful content.
fn render_horizontal_viewport(rows: &[ViewportRow], width: usize) -> Vec<String> {
    let viewport_width = width.saturating_sub(TREE_GUTTER_WIDTH);
    let max_body_width = rows.iter().map(|row| row.body_width).max().unwrap_or(0);
    let max_horizontal_scroll = max_body_width.saturating_sub(viewport_width);
    let selected_row = rows.iter().find(|row| row.is_selected);

    let mut horizontal_scroll = 0;
    if let Some(selected) = selected_row
        && max_horizontal_scroll > 0
    {
        let min_visible_anchor_content_width = MAX_VISIBLE_ANCHOR_CONTENT_WIDTH
            .min(MIN_VISIBLE_ANCHOR_CONTENT_WIDTH.max(viewport_width / 3));
        if selected.anchor_col > viewport_width.saturating_sub(min_visible_anchor_content_width) {
            let anchor_context_width =
                MAX_ANCHOR_CONTEXT_WIDTH.min(MIN_ANCHOR_CONTEXT_WIDTH.max(viewport_width / 4));
            horizontal_scroll =
                max_horizontal_scroll.min(selected.anchor_col.saturating_sub(anchor_context_width));
        }
    }

    rows.iter()
        .map(|row| {
            let line = if horizontal_scroll > 0 {
                format!(
                    "{}{}\x1b[0m",
                    row.gutter,
                    slice_by_column(&row.body, horizontal_scroll, viewport_width, true)
                )
            } else {
                format!("{}{}", row.gutter, row.body)
            };
            truncate_to_width(&line, width, "", false)
        })
        .collect()
}

/// The flatten walk's stack item: [node, indent, justBranched,
/// showConnector, isLast, gutters, isVirtualRootChild]
/// (`tree-selector.ts:212-213`).
type FlattenStackItem<'a> = (
    &'a SessionTreeNode,
    usize,
    bool,
    bool,
    bool,
    Vec<GutterInfo>,
    bool,
);

/// The filtered-view walk's stack item: [nodeId, indent, justBranched,
/// showConnector, isLast, gutters, isVirtualRootChild]
/// (`tree-selector.ts:491-492`).
type RecalcStackItem = (String, usize, bool, bool, bool, Vec<GutterInfo>, bool);

/// Tool call info for lookup (`ToolCallInfo`, `tree-selector.ts:104-107`).
struct ToolCallInfo {
    name: String,
    arguments: serde_json::Map<String, serde_json::Value>,
}

/// The tree list with selection, filters, folding, and search
/// (`TreeList`, `tree-selector.ts:109-1153`).
pub struct TreeList {
    flat_nodes: Vec<FlatNode>,
    /// Indices into `flat_nodes`, in display order.
    filtered_nodes: Vec<usize>,
    selected_index: usize,
    current_leaf_id: Option<String>,
    max_visible_lines: usize,
    filter_mode: TreeFilterMode,
    search_query: String,
    tool_call_map: HashMap<String, ToolCallInfo>,
    multiple_roots: bool,
    show_label_timestamps: bool,
    active_path_ids: HashSet<String>,
    visible_parent_map: HashMap<String, Option<String>>,
    visible_children_map: HashMap<Option<String>, Vec<String>>,
    last_selected_id: Option<String>,
    folded_nodes: HashSet<String>,
}

impl TreeList {
    fn new(
        tree: &[SessionTreeNode],
        current_leaf_id: Option<&str>,
        max_visible_lines: usize,
        initial_selected_id: Option<&str>,
        initial_filter_mode: TreeFilterMode,
    ) -> Self {
        let mut list = Self {
            flat_nodes: Vec::new(),
            filtered_nodes: Vec::new(),
            selected_index: 0,
            current_leaf_id: current_leaf_id.map(str::to_string),
            max_visible_lines,
            filter_mode: initial_filter_mode,
            search_query: String::new(),
            tool_call_map: HashMap::new(),
            multiple_roots: tree.len() > 1,
            show_label_timestamps: false,
            active_path_ids: HashSet::new(),
            visible_parent_map: HashMap::new(),
            visible_children_map: HashMap::new(),
            last_selected_id: None,
            folded_nodes: HashSet::new(),
        };
        list.flat_nodes = list.flatten_tree(tree);
        list.build_active_path();
        list.apply_filter();

        // Start with initialSelectedId if provided, otherwise the current leaf
        // (`tree-selector.ts:147-149`).
        let target_id = initial_selected_id.or(current_leaf_id);
        list.selected_index = list.find_nearest_visible_index(target_id);
        list.last_selected_id = list
            .filtered_nodes
            .get(list.selected_index)
            .map(|&index| list.flat_nodes[index].node.entry.id().to_string());
        list
    }

    /// The index of the nearest visible entry, walking up the parent chain
    /// (`findNearestVisibleIndex`, `tree-selector.ts:156-180`); the last
    /// visible index is the fallback.
    fn find_nearest_visible_index(&self, entry_id: Option<&str>) -> usize {
        if self.filtered_nodes.is_empty() {
            return 0;
        }

        // Parent lookup over the full flat list.
        let parent_of: HashMap<&str, Option<&str>> = self
            .flat_nodes
            .iter()
            .map(|flat| (flat.node.entry.id(), flat.node.entry.parent_id()))
            .collect();

        // Visible entry ids to their indices in filteredNodes.
        let visible_id_to_index: HashMap<&str, usize> = self
            .filtered_nodes
            .iter()
            .enumerate()
            .map(|(index, &flat_index)| (self.flat_nodes[flat_index].node.entry.id(), index))
            .collect();

        let mut current_id = entry_id;
        while let Some(id) = current_id {
            if let Some(&index) = visible_id_to_index.get(id) {
                return index;
            }
            current_id = parent_of.get(id).copied().flatten();
        }
        self.filtered_nodes.len() - 1
    }

    /// The entry ids on the path from root to the current leaf
    /// (`buildActivePath`, `tree-selector.ts:183-201`).
    fn build_active_path(&mut self) {
        self.active_path_ids.clear();
        let Some(mut current_id) = self.current_leaf_id.clone() else {
            return;
        };

        let parent_of: HashMap<String, Option<String>> = self
            .flat_nodes
            .iter()
            .map(|flat| {
                (
                    flat.node.entry.id().to_string(),
                    flat.node
                        .entry
                        .parent_id()
                        .map(std::string::ToString::to_string),
                )
            })
            .collect();

        loop {
            self.active_path_ids.insert(current_id.clone());
            match parent_of.get(&current_id) {
                Some(Some(parent)) => current_id = parent.clone(),
                // A missing key or a null parent end the walk.
                _ => break,
            }
        }
    }

    /// `flattenTree` (`tree-selector.ts:203-331`): the pre-order flat list
    /// with pi's indentation rules — a single-child chain stays flat; a
    /// branch point and the first generation after one shift right. The
    /// branch containing the active leaf sorts first at every branch point.
    #[allow(clippy::too_many_lines)] // mirrors the TS flattenTree; split when extending
    fn flatten_tree(&mut self, roots: &[SessionTreeNode]) -> Vec<FlatNode> {
        let mut result: Vec<FlatNode> = Vec::new();
        self.tool_call_map.clear();

        // Which subtrees contain the active leaf, by id (iterative post-order
        // over the nested nodes, `tree-selector.ts:217-243`).
        let mut contains_active: HashMap<String, bool> = HashMap::new();
        let mut all_nodes: Vec<&SessionTreeNode> = Vec::new();
        let mut pre_order_stack: Vec<&SessionTreeNode> = roots.iter().collect();
        while let Some(node) = pre_order_stack.pop() {
            all_nodes.push(node);
            for child in node.children.iter().rev() {
                pre_order_stack.push(child);
            }
        }
        for node in all_nodes.iter().rev() {
            let mut has = self
                .current_leaf_id
                .as_deref()
                .is_some_and(|leaf| leaf == node.entry.id());
            for child in &node.children {
                if contains_active
                    .get(child.entry.id())
                    .copied()
                    .unwrap_or(false)
                {
                    has = true;
                }
            }
            contains_active.insert(node.entry.id().to_string(), has);
        }

        // Stack items: [node, indent, justBranched, showConnector, isLast,
        // gutters, isVirtualRootChild].
        let mut stack: Vec<FlattenStackItem> = Vec::new();

        // Roots ordered so the one containing the active leaf comes first;
        // multiple roots render as children of a virtual branching root
        // (`tree-selector.ts:245-252`).
        let multiple_roots = roots.len() > 1;
        let mut ordered_roots: Vec<&SessionTreeNode> = roots.iter().collect();
        ordered_roots.sort_by_key(|node| {
            let active = contains_active
                .get(node.entry.id())
                .copied()
                .unwrap_or(false);
            !active as u8
        });
        for (index, node) in ordered_roots.iter().enumerate().rev() {
            let is_last = index == ordered_roots.len() - 1;
            stack.push((
                node,
                usize::from(multiple_roots),
                multiple_roots,
                multiple_roots,
                is_last,
                Vec::new(),
                multiple_roots,
            ));
        }

        while let Some(item) = stack.pop() {
            let (
                node,
                indent,
                just_branched,
                show_connector,
                is_last,
                gutters,
                is_virtual_root_child,
            ) = item;

            // Extract tool calls from assistant messages for later lookup
            // (`tree-selector.ts:257-269`).
            if let SessionEntry::Message(message) = &node.entry
                && let pi_agent::AgentMessage::Assistant(assistant) = &message.message
            {
                for block in &assistant.content {
                    if let pi_ai::types::AssistantContent::ToolCall(call) = block {
                        self.tool_call_map.insert(
                            call.id.clone(),
                            ToolCallInfo {
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                        );
                    }
                }
            }

            result.push(FlatNode {
                node: node.clone(),
                indent,
                show_connector,
                is_last,
                gutters: gutters.clone(),
                is_virtual_root_child,
            });

            let children = &node.children;
            let multiple_children = children.len() > 1;

            // Children ordered so the branch containing the active leaf comes
            // first (`tree-selector.ts:276-288`).
            let mut prioritized: Vec<&SessionTreeNode> = Vec::new();
            let mut rest: Vec<&SessionTreeNode> = Vec::new();
            for child in children {
                let active = contains_active
                    .get(child.entry.id())
                    .copied()
                    .unwrap_or(false);
                if active {
                    prioritized.push(child);
                } else {
                    rest.push(child);
                }
            }
            prioritized.extend(rest);

            // Child indent: a branch point and the first generation after one
            // shift right (`tree-selector.ts:290-301`).
            let child_indent = if multiple_children || (just_branched && indent > 0) {
                indent + 1
            } else {
                indent
            };

            // A displayed connector adds a gutter entry for descendants at
            // the connector's position (`tree-selector.ts:303-313`).
            let connector_displayed = show_connector && !is_virtual_root_child;
            let current_display_indent = if self.multiple_roots {
                indent.saturating_sub(1)
            } else {
                indent
            };
            let connector_position = current_display_indent.saturating_sub(1);
            let mut child_gutters = gutters;
            if connector_displayed {
                child_gutters.push(GutterInfo {
                    position: connector_position,
                    show: !is_last,
                });
            }

            for index in (0..prioritized.len()).rev() {
                let child_is_last = index == prioritized.len() - 1;
                stack.push((
                    prioritized[index],
                    child_indent,
                    multiple_children,
                    multiple_children,
                    child_is_last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }

        result
    }

    /// `applyFilter` (`tree-selector.ts:333-431`): filter the flat list, drop
    /// folded descendants, recompute the visual structure, and restore the
    /// selection (or its nearest visible ancestor).
    #[allow(clippy::too_many_lines)] // mirrors the TS applyFilter; split when extending
    fn apply_filter(&mut self) {
        // Preserve the selection through empty filter results
        // (`tree-selector.ts:334-338`).
        if !self.filtered_nodes.is_empty() {
            self.last_selected_id = self
                .filtered_nodes
                .get(self.selected_index)
                .and_then(|&index| {
                    self.flat_nodes
                        .get(index)
                        .map(|flat| flat.node.entry.id().to_string())
                })
                .or(self.last_selected_id.clone());
        }

        let search_tokens: Vec<String> = self
            .search_query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_string)
            .collect();

        let mut filtered: Vec<usize> = Vec::new();
        for (index, flat) in self.flat_nodes.iter().enumerate() {
            let entry = &flat.node.entry;
            if entry.kind() == "usage" {
                continue;
            }
            let is_current_leaf = self.current_leaf_id.as_deref() == Some(entry.id());

            // Skip assistant messages with only tool calls (no text) unless
            // error/aborted; always show the current leaf
            // (`tree-selector.ts:347-357`).
            if let SessionEntry::Message(message) = entry
                && let pi_agent::AgentMessage::Assistant(assistant) = &message.message
                && !is_current_leaf
            {
                let has_text = has_text_content_assistant(&assistant.content);
                let is_error_or_aborted = !matches!(
                    assistant.stop_reason,
                    pi_ai::types::StopReason::Stop | pi_ai::types::StopReason::ToolUse
                );
                if !has_text && !is_error_or_aborted {
                    continue;
                }
            }

            // Entry types hidden in the default view (settings/bookkeeping)
            // (`tree-selector.ts:361-368`).
            let is_settings_entry = matches!(
                entry,
                SessionEntry::Label(_)
                    | SessionEntry::ContextEdit(_)
                    | SessionEntry::Custom(_)
                    | SessionEntry::ModelChange(_)
                    | SessionEntry::ThinkingLevelChange(_)
                    | SessionEntry::SessionInfo(_)
            );

            let passes_filter = match self.filter_mode {
                TreeFilterMode::UserOnly => {
                    matches!(
                        entry,
                        SessionEntry::Message(message)
                            if matches!(&message.message, pi_agent::AgentMessage::User(_))
                    )
                }
                TreeFilterMode::NoTools => {
                    !is_settings_entry
                        && !matches!(
                            entry,
                            SessionEntry::Message(message)
                                if matches!(&message.message, pi_agent::AgentMessage::ToolResult(_))
                        )
                }
                TreeFilterMode::LabeledOnly => flat.node.label.is_some(),
                TreeFilterMode::All => true,
                TreeFilterMode::Default => !is_settings_entry,
            };
            if !passes_filter {
                continue;
            }

            if !search_tokens.is_empty() {
                let node_text = Self::get_searchable_text(flat).to_lowercase();
                if !search_tokens.iter().all(|token| node_text.contains(token)) {
                    continue;
                }
            }

            filtered.push(index);
        }

        // Filter out descendants of folded nodes (`tree-selector.ts:404-414`).
        if !self.folded_nodes.is_empty() {
            let mut skip_set: HashSet<String> = HashSet::new();
            for flat in &self.flat_nodes {
                let id = flat.node.entry.id().to_string();
                if let Some(parent_id) = flat.node.entry.parent_id()
                    && (self.folded_nodes.contains(parent_id) || skip_set.contains(parent_id))
                {
                    skip_set.insert(id);
                }
            }
            filtered.retain(|&index| !skip_set.contains(self.flat_nodes[index].node.entry.id()));
        }
        self.filtered_nodes = filtered;

        self.recalculate_visual_structure();

        // Preserve the cursor on the same node, or find the nearest visible
        // ancestor (`tree-selector.ts:419-430`).
        if let Some(last) = self.last_selected_id.clone() {
            self.selected_index = self.find_nearest_visible_index(Some(&last));
        } else if self.selected_index >= self.filtered_nodes.len() {
            self.selected_index = self.filtered_nodes.len().saturating_sub(1);
        }
        if !self.filtered_nodes.is_empty() {
            self.last_selected_id = self
                .filtered_nodes
                .get(self.selected_index)
                .and_then(|&index| {
                    self.flat_nodes
                        .get(index)
                        .map(|flat| flat.node.entry.id().to_string())
                })
                .or(self.last_selected_id.clone());
        }
    }

    /// `recalculateVisualStructure` (`tree-selector.ts:439-562`): recompute
    /// indentation/connectors for the filtered view; descendants attach to
    /// the nearest visible ancestor.
    #[allow(clippy::too_many_lines)] // mirrors the TS recalculateVisualStructure; split when extending
    fn recalculate_visual_structure(&mut self) {
        if self.filtered_nodes.is_empty() {
            return;
        }

        let visible_ids: HashSet<String> = self
            .filtered_nodes
            .iter()
            .map(|&index| self.flat_nodes[index].node.entry.id().to_string())
            .collect();

        // Full-tree parent lookup (`tree-selector.ts:444-448`).
        let parent_of: HashMap<String, Option<String>> = self
            .flat_nodes
            .iter()
            .map(|flat| {
                (
                    flat.node.entry.id().to_string(),
                    flat.node
                        .entry
                        .parent_id()
                        .map(std::string::ToString::to_string),
                )
            })
            .collect();

        let find_visible_ancestor = |node_id: &str| -> Option<String> {
            let mut current_id = parent_of.get(node_id).cloned().flatten();
            while let Some(id) = current_id {
                if visible_ids.contains(&id) {
                    return Some(id);
                }
                current_id = parent_of.get(&id).cloned().flatten();
            }
            None
        };

        // The visible tree: node → nearest visible ancestor; parent → its
        // visible children in filtered order (`tree-selector.ts:462-478`).
        let mut visible_parent: HashMap<String, Option<String>> = HashMap::new();
        let mut visible_children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        visible_children.insert(None, Vec::new());

        for &index in &self.filtered_nodes {
            let node_id = self.flat_nodes[index].node.entry.id().to_string();
            let ancestor_id = find_visible_ancestor(&node_id);
            visible_parent.insert(node_id.clone(), ancestor_id.clone());
            visible_children
                .entry(ancestor_id)
                .or_default()
                .push(node_id);
        }

        // `multipleRoots` follows the visible roots
        // (`tree-selector.ts:480-482`).
        let visible_root_ids = visible_children.get(&None).cloned().unwrap_or_default();
        self.multiple_roots = visible_root_ids.len() > 1;

        // DFS over the visible tree with the flattenTree indentation rules
        // (`tree-selector.ts:490-557`).
        let mut stack: Vec<RecalcStackItem> = Vec::new();
        for (index, root_id) in visible_root_ids.iter().enumerate().rev() {
            let is_last = index == visible_root_ids.len() - 1;
            stack.push((
                root_id.clone(),
                usize::from(self.multiple_roots),
                self.multiple_roots,
                self.multiple_roots,
                is_last,
                Vec::new(),
                self.multiple_roots,
            ));
        }

        while let Some(item) = stack.pop() {
            let (
                node_id,
                indent,
                just_branched,
                show_connector,
                is_last,
                gutters,
                is_virtual_root_child,
            ) = item;

            let Some(flat_index) = self
                .filtered_nodes
                .iter()
                .copied()
                .find(|&index| self.flat_nodes[index].node.entry.id() == node_id)
            else {
                continue;
            };

            // Update this node's visual properties (`:515-520`).
            {
                let flat = &mut self.flat_nodes[flat_index];
                flat.indent = indent;
                flat.show_connector = show_connector;
                flat.is_last = is_last;
                flat.gutters.clone_from(&gutters);
                flat.is_virtual_root_child = is_virtual_root_child;
            }

            let children = visible_children
                .get(&Some(node_id.clone()))
                .cloned()
                .unwrap_or_default();
            let multiple_children = children.len() > 1;

            let child_indent = if multiple_children || (just_branched && indent > 0) {
                indent + 1
            } else {
                indent
            };

            let connector_displayed = show_connector && !is_virtual_root_child;
            let current_display_indent = if self.multiple_roots {
                indent.saturating_sub(1)
            } else {
                indent
            };
            let connector_position = current_display_indent.saturating_sub(1);
            let mut child_gutters = gutters;
            if connector_displayed {
                child_gutters.push(GutterInfo {
                    position: connector_position,
                    show: !is_last,
                });
            }

            for index in (0..children.len()).rev() {
                let child_is_last = index == children.len() - 1;
                stack.push((
                    children[index].clone(),
                    child_indent,
                    multiple_children,
                    multiple_children,
                    child_is_last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }

        // Stored for the ancestor/descendant lookups in navigation
        // (`tree-selector.ts:559-561`).
        self.visible_parent_map = visible_parent;
        self.visible_children_map = visible_children;
    }

    /// `getSearchableText` (`tree-selector.ts:564-623`): label, role, and
    /// content pieces joined with spaces.
    fn get_searchable_text(flat: &FlatNode) -> String {
        let node = &flat.node;
        let entry = &node.entry;
        let mut parts: Vec<String> = Vec::new();

        if let Some(label) = &node.label {
            parts.push(label.clone());
        }

        match entry {
            SessionEntry::Message(message) => {
                parts.push(message.message.role().to_string());
                match &message.message {
                    pi_agent::AgentMessage::User(user) => {
                        parts.push(extract_user_content(user));
                    }
                    pi_agent::AgentMessage::Assistant(assistant) => {
                        parts.push(extract_assistant_content(&assistant.content));
                    }
                    pi_agent::AgentMessage::ToolResult(result) => {
                        parts.push(extract_input_content(&result.content));
                    }
                    pi_agent::AgentMessage::BashExecution(bash) => {
                        parts.push(bash.command.clone());
                    }
                    _ => {}
                }
            }
            SessionEntry::CustomMessage(custom) => {
                parts.push(custom.custom_type.clone());
                parts.push(extract_user_content_value(&custom.content));
            }
            SessionEntry::Compaction(_) => parts.push("compaction".to_string()),
            SessionEntry::BranchSummary(summary) => {
                parts.push("branch summary".to_string());
                parts.push(summary.summary.clone());
            }
            SessionEntry::SessionInfo(info) => {
                parts.push("title".to_string());
                if let Some(name) = &info.name {
                    parts.push(name.clone());
                }
            }
            SessionEntry::ModelChange(change) => {
                parts.push("model".to_string());
                parts.push(change.model_id.clone());
            }
            SessionEntry::ThinkingLevelChange(change) => {
                parts.push("thinking".to_string());
                parts.push(change.thinking_level.clone());
            }
            SessionEntry::Custom(custom) => {
                parts.push("custom".to_string());
                parts.push(custom.custom_type.clone());
            }
            SessionEntry::ContextEdit(edit) => {
                parts.push("context edit".to_string());
                parts.push(replacement_label(&edit.replacement).to_string());
                parts.push(edit.target_id.clone());
            }
            SessionEntry::Label(label) => {
                parts.push("label".to_string());
                parts.push(label.label.clone().unwrap_or_default());
            }
            _ => {}
        }

        parts.join(" ")
    }

    /// `getSelectedNode` (`tree-selector.ts:631-633`).
    fn get_selected_node(&self) -> Option<&SessionTreeNode> {
        self.filtered_nodes
            .get(self.selected_index)
            .map(|&index| &self.flat_nodes[index].node)
    }

    /// `copySelected` (`tree-selector.ts:635-638`).
    fn copy_selected(&self) -> Option<String> {
        self.get_selected_node().and_then(Self::get_entry_copy_text)
    }

    /// `updateNodeLabel` (`tree-selector.ts:640-648`).
    fn update_node_label(&mut self, entry_id: &str, label: Option<&str>) {
        for flat in &mut self.flat_nodes {
            if flat.node.entry.id() == entry_id {
                flat.node.label = label.map(str::to_string);
                flat.node.label_timestamp = label.map(|_| now_iso8601());
                break;
            }
        }
    }

    /// `getStatusLabels` (`tree-selector.ts:650-670`): the mode suffix plus
    /// the label-timestamp flag.
    fn get_status_labels(&self) -> String {
        let mut labels = self.filter_mode.status_label().to_string();
        if self.show_label_timestamps {
            labels.push_str(" [+label time]");
        }
        labels
    }

    /// `render` (`tree-selector.ts:672-774`).
    #[allow(clippy::too_many_lines)] // mirrors the TS render; split when extending
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let mut lines: Vec<String> = Vec::new();

        if self.filtered_nodes.is_empty() {
            lines.push(truncate_to_width(
                &theme.fg(ThemeColor::Muted, "  No entries found"),
                width,
                "",
                false,
            ));
            let counter = format!("  (0/0){}", self.get_status_labels());
            lines.push(truncate_to_width(
                &theme.fg(ThemeColor::Muted, &counter),
                width,
                "",
                false,
            ));
            return lines;
        }

        // Visible range centered on the selection (`tree-selector.ts:681-688`).
        let start_index = self
            .selected_index
            .saturating_sub(self.max_visible_lines / 2)
            .min(
                self.filtered_nodes
                    .len()
                    .saturating_sub(self.max_visible_lines),
            );
        let end_index = (start_index + self.max_visible_lines).min(self.filtered_nodes.len());

        let mut rendered_rows: Vec<ViewportRow> = Vec::new();
        for index in start_index..end_index {
            let flat_index = self.filtered_nodes[index];
            let flat = &self.flat_nodes[flat_index];
            let entry = &flat.node.entry;
            let is_selected = index == self.selected_index;

            let cursor = if is_selected {
                theme.fg(ThemeColor::Accent, "› ")
            } else {
                "  ".to_string()
            };

            // With multiple roots, display indents shift so roots sit at 0
            // (`tree-selector.ts:699-700`).
            let display_indent = if self.multiple_roots {
                flat.indent.saturating_sub(1)
            } else {
                flat.indent
            };

            // Prefix built char by char, placing gutters and the connector at
            // their positions, with the fold indicator in the connector
            // (`tree-selector.ts:708-738`).
            let total_chars = display_indent * 3;
            let is_folded = self.folded_nodes.contains(entry.id());
            let connector = if flat.show_connector && !flat.is_virtual_root_child {
                if flat.is_last { "└─ " } else { "├─ " }
            } else {
                ""
            };
            let connector_position = if connector.is_empty() {
                None
            } else {
                Some(display_indent.saturating_sub(1))
            };
            let mut prefix_chars: Vec<&str> = Vec::with_capacity(total_chars);
            for i in 0..total_chars {
                let level = i / 3;
                let pos_in_level = i % 3;
                if let Some(gutter) = flat.gutters.iter().find(|g| g.position == level) {
                    if pos_in_level == 0 {
                        prefix_chars.push(if gutter.show { "│" } else { " " });
                    } else {
                        prefix_chars.push(" ");
                    }
                } else if Some(level) == connector_position {
                    if pos_in_level == 0 {
                        prefix_chars.push(if flat.is_last { "└" } else { "├" });
                    } else if pos_in_level == 1 {
                        let foldable = self.is_foldable(entry.id());
                        prefix_chars.push(if is_folded {
                            "⊞"
                        } else if foldable {
                            "⊟"
                        } else {
                            "─"
                        });
                    } else {
                        prefix_chars.push(" ");
                    }
                } else {
                    prefix_chars.push(" ");
                }
            }
            let prefix = prefix_chars.concat();

            // Fold marker for nodes without connectors (roots)
            // (`tree-selector.ts:740-742`).
            let shows_fold_in_connector = flat.show_connector && !flat.is_virtual_root_child;
            let fold_marker = if is_folded && !shows_fold_in_connector {
                theme.fg(ThemeColor::Accent, "⊞ ")
            } else {
                String::new()
            };

            // The active-path marker sits right before the entry text
            // (`tree-selector.ts:744-746`).
            let is_on_active_path = self.active_path_ids.contains(entry.id());
            let path_marker = if is_on_active_path {
                theme.fg(ThemeColor::Accent, "• ")
            } else {
                String::new()
            };

            let label = match &flat.node.label {
                Some(label) => theme.fg(ThemeColor::Warning, &format!("[{label}] ")),
                None => String::new(),
            };
            let label_timestamp = match (
                self.show_label_timestamps,
                &flat.node.label,
                &flat.node.label_timestamp,
            ) {
                (true, Some(_), Some(timestamp)) => theme.fg(
                    ThemeColor::Muted,
                    &format!("{} ", format_label_timestamp(timestamp)),
                ),
                _ => String::new(),
            };
            let content = self.get_entry_display_text(flat, is_selected);
            let prefix_part = theme.fg(ThemeColor::Dim, &prefix) + &fold_marker + &path_marker;
            let anchor_col = visible_width(&prefix_part);
            let mut gutter = cursor;
            let mut body = format!("{prefix_part}{label}{label_timestamp}{content}");
            if is_selected {
                gutter = theme.bg(ThemeBg::SelectedBg, &gutter);
                body = theme.bg(ThemeBg::SelectedBg, &body);
            }
            // Width read before `body` moves into the row (a struct literal
            // argument cannot borrow a field's own source afterwards).
            let body_width = visible_width(&body);
            rendered_rows.push(ViewportRow {
                gutter,
                body,
                anchor_col,
                body_width,
                is_selected,
            });
        }

        lines.extend(render_horizontal_viewport(&rendered_rows, width));
        let counter = format!(
            "  ({}/{}){}",
            self.selected_index + 1,
            self.filtered_nodes.len(),
            self.get_status_labels()
        );
        lines.push(truncate_to_width(
            &theme.fg(ThemeColor::Muted, &counter),
            width,
            "",
            false,
        ));

        lines
    }

    /// `getEntryDisplayText` (`tree-selector.ts:776-863`): the per-type row
    /// body, bolded for the selection.
    fn get_entry_display_text(&self, flat: &FlatNode, is_selected: bool) -> String {
        let theme = theme::theme();
        let entry = &flat.node.entry;
        let normalize = |s: &str| -> String { s.replace(['\n', '\t'], " ").trim().to_string() };

        let result = match entry {
            SessionEntry::Message(message) => match &message.message {
                pi_agent::AgentMessage::User(user) => {
                    format!(
                        "{}{}",
                        theme.fg(ThemeColor::Accent, "user: "),
                        normalize(&extract_user_content(user))
                    )
                }
                pi_agent::AgentMessage::Assistant(assistant) => {
                    let text_content = normalize(&extract_assistant_content(&assistant.content));
                    if !text_content.is_empty() {
                        theme.fg(ThemeColor::Success, "assistant: ") + &text_content
                    } else if assistant.stop_reason == pi_ai::types::StopReason::Aborted {
                        theme.fg(ThemeColor::Success, "assistant: ")
                            + &theme.fg(ThemeColor::Muted, "(aborted)")
                    } else if let Some(error_message) = &assistant.error_message {
                        let error: String = normalize(error_message).chars().take(80).collect();
                        theme.fg(ThemeColor::Success, "assistant: ")
                            + &theme.fg(ThemeColor::Error, &error)
                    } else {
                        theme.fg(ThemeColor::Success, "assistant: ")
                            + &theme.fg(ThemeColor::Muted, "(no content)")
                    }
                }
                pi_agent::AgentMessage::ToolResult(result) => {
                    match self.tool_call_map.get(&result.tool_call_id) {
                        Some(tool_call) => theme.fg(
                            ThemeColor::Muted,
                            &format_tool_call(&tool_call.name, &tool_call.arguments),
                        ),
                        None => theme.fg(ThemeColor::Muted, &format!("[{}]", result.tool_name)),
                    }
                }
                pi_agent::AgentMessage::BashExecution(bash) => theme.fg(
                    ThemeColor::Dim,
                    &format!("[bash]: {}", normalize(&bash.command)),
                ),
                other => theme.fg(ThemeColor::Dim, &format!("[{}]", other.role())),
            },
            SessionEntry::CustomMessage(custom) => {
                let content = extract_user_content_value(&custom.content);
                theme.fg(
                    ThemeColor::CustomMessageLabel,
                    &format!("[{}]: ", custom.custom_type),
                ) + &normalize(&content)
            }
            SessionEntry::Compaction(compaction) => {
                let tokens = (compaction.tokens_before / 1000.0).round() as i64;
                theme.fg(
                    ThemeColor::BorderAccent,
                    &format!("[compaction: {tokens}k tokens]"),
                )
            }
            SessionEntry::BranchSummary(summary) => {
                theme.fg(ThemeColor::Warning, "[branch summary]: ") + &normalize(&summary.summary)
            }
            SessionEntry::ModelChange(change) => {
                theme.fg(ThemeColor::Dim, &format!("[model: {}]", change.model_id))
            }
            SessionEntry::ThinkingLevelChange(change) => theme.fg(
                ThemeColor::Dim,
                &format!("[thinking: {}]", change.thinking_level),
            ),
            SessionEntry::Custom(custom) => theme.fg(
                ThemeColor::Dim,
                &format!("[custom: {}]", custom.custom_type),
            ),
            SessionEntry::ContextEdit(edit) => theme.fg(
                ThemeColor::Dim,
                &format!(
                    "[context {}: {}]",
                    replacement_label(&edit.replacement),
                    edit.target_id
                ),
            ),
            SessionEntry::Label(label) => theme.fg(
                ThemeColor::Dim,
                &format!("[label: {}]", label.label.as_deref().unwrap_or("(cleared)")),
            ),
            SessionEntry::SessionInfo(info) => match &info.name {
                Some(name) => theme.fg(ThemeColor::Dim, &format!("[title: {name}]")),
                None => theme.fg(
                    ThemeColor::Dim,
                    &format!("[title: {}]", theme::italic("empty")),
                ),
            },
            _ => String::new(),
        };

        if is_selected {
            theme::bold(&result)
        } else {
            result
        }
    }

    /// `getEntryCopyText` (`tree-selector.ts:907-934`): the full text of the
    /// selected entry, or `None` when it has none.
    fn get_entry_copy_text(node: &SessionTreeNode) -> Option<String> {
        let text = match &node.entry {
            SessionEntry::Message(message) => match &message.message {
                pi_agent::AgentMessage::BashExecution(bash) => Some(bash.command.clone()),
                pi_agent::AgentMessage::User(user) => Some(extract_user_content(user)),
                pi_agent::AgentMessage::Assistant(assistant) => {
                    let text = extract_assistant_content(&assistant.content);
                    if text.is_empty() {
                        assistant.error_message.clone()
                    } else {
                        Some(text)
                    }
                }
                pi_agent::AgentMessage::ToolResult(result) => {
                    Some(extract_input_content(&result.content))
                }
                _ => None,
            },
            SessionEntry::CustomMessage(custom) => {
                Some(extract_user_content_value(&custom.content))
            }
            SessionEntry::Compaction(compaction) => Some(compaction.summary.clone()),
            SessionEntry::BranchSummary(summary) => Some(summary.summary.clone()),
            _ => None,
        };
        text.filter(|text| !text.trim().is_empty())
    }

    /// `formatToolCall` (`tree-selector.ts:949-993`): the known tools render
    /// path-centric summaries; anything else shows truncated JSON args.
    fn format_tool_call(
        name: &str,
        arguments: &serde_json::Map<String, serde_json::Value>,
    ) -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        let shorten = |key: &str, fallback: &str| -> String {
            let raw = arguments
                .get(key)
                .or_else(|| arguments.get(fallback))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            shorten_home_path(raw, &home)
        };
        let arg_str = |key: &str| -> String {
            arguments
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match name {
            "read" => format!(
                "[read: {}{}]",
                shorten("path", "file_path"),
                format_read_line_range(arguments.get("offset"), arguments.get("limit"))
            ),
            "write" => format!("[write: {}]", shorten("path", "file_path")),
            "edit" => format!("[edit: {}]", shorten("path", "file_path")),
            "bash" => {
                let raw_cmd = arg_str("command");
                let cmd: String = raw_cmd
                    .replace(['\n', '\t'], " ")
                    .trim()
                    .chars()
                    .take(50)
                    .collect();
                let ellipsis = if raw_cmd.chars().count() > 50 {
                    "..."
                } else {
                    ""
                };
                format!("[bash: {cmd}{ellipsis}]")
            }
            "grep" => format!(
                "[grep: /{}/ in {}]",
                arg_str("pattern"),
                shorten("path", ".")
            ),
            "find" => format!("[find: {} in {}]", arg_str("pattern"), shorten("path", ".")),
            "ls" => format!("[ls: {}]", shorten("path", ".")),
            _ => {
                let args = serde_json::Value::Object(arguments.clone()).to_string();
                let args_preview: String = args.chars().take(40).collect();
                let ellipsis = if args.chars().count() > 40 { "..." } else { "" };
                format!("[{name}: {args_preview}{ellipsis}]")
            }
        }
    }

    /// `isFoldable` (`tree-selector.ts:1108-1115`): visible children, and
    /// either a root or the start of a branch segment.
    fn is_foldable(&self, entry_id: &str) -> bool {
        let Some(children) = self.visible_children_map.get(&Some(entry_id.to_string())) else {
            return false;
        };
        if children.is_empty() {
            return false;
        }
        match self.visible_parent_map.get(entry_id) {
            None | Some(None) => true,
            Some(Some(parent_id)) => self
                .visible_children_map
                .get(&Some(parent_id.clone()))
                .is_some_and(|siblings| siblings.len() > 1),
        }
    }

    /// `findBranchSegmentStart` (`tree-selector.ts:1124-1152`): the next
    /// branch-segment start in the given direction. "up" walks the visible
    /// parent chain; "down" walks the first visible child.
    fn find_branch_segment_start(&self, direction: &str) -> usize {
        let Some(&selected_index) = self.filtered_nodes.get(self.selected_index) else {
            return self.selected_index;
        };
        let selected_id = self.flat_nodes[selected_index].node.entry.id().to_string();

        let index_by_entry_id: HashMap<&str, usize> = self
            .filtered_nodes
            .iter()
            .enumerate()
            .map(|(index, &flat_index)| (self.flat_nodes[flat_index].node.entry.id(), index))
            .collect();
        let mut current_id = selected_id;
        if direction == "down" {
            loop {
                let children = self
                    .visible_children_map
                    .get(&Some(current_id.clone()))
                    .cloned()
                    .unwrap_or_default();
                if children.is_empty() {
                    return index_by_entry_id
                        .get(current_id.as_str())
                        .copied()
                        .unwrap_or(self.selected_index);
                }
                if children.len() > 1 {
                    return index_by_entry_id
                        .get(children[0].as_str())
                        .copied()
                        .unwrap_or(self.selected_index);
                }
                current_id.clone_from(&children[0]);
            }
        }

        // direction === "up"
        loop {
            let parent_id = self.visible_parent_map.get(&current_id).cloned().flatten();
            let Some(parent_id) = parent_id else {
                return index_by_entry_id
                    .get(current_id.as_str())
                    .copied()
                    .unwrap_or(self.selected_index);
            };
            let children = self
                .visible_children_map
                .get(&Some(parent_id.clone()))
                .cloned()
                .unwrap_or_default();
            if children.len() > 1
                && let Some(segment_start) = index_by_entry_id.get(current_id.as_str()).copied()
                && segment_start < self.selected_index
            {
                return segment_start;
            }
            current_id = parent_id;
        }
    }

    /// `handleInput` (`tree-selector.ts:995-1101`): the checks and their
    /// order are upstream's.
    #[allow(clippy::too_many_lines)] // mirrors the TS handleInput; split when extending
    fn handle_input(&mut self, data: &str) -> TreeListOutcome {
        if keybindings::matches(data, action::SELECT_UP) {
            self.selected_index = if self.selected_index == 0 {
                self.filtered_nodes.len().saturating_sub(1)
            } else {
                self.selected_index - 1
            };
            TreeListOutcome::None
        } else if keybindings::matches(data, action::SELECT_DOWN) {
            self.selected_index =
                if self.selected_index == self.filtered_nodes.len().saturating_sub(1) {
                    0
                } else {
                    self.selected_index + 1
                };
            TreeListOutcome::None
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_FOLD_OR_UP) {
            let current_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|&index| self.flat_nodes[index].node.entry.id().to_string());
            match current_id {
                Some(current_id)
                    if self.is_foldable(&current_id)
                        && !self.folded_nodes.contains(&current_id) =>
                {
                    self.folded_nodes.insert(current_id);
                    self.apply_filter();
                }
                _ => {
                    self.selected_index = self.find_branch_segment_start("up");
                }
            }
            TreeListOutcome::None
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_UNFOLD_OR_DOWN) {
            let current_id = self
                .filtered_nodes
                .get(self.selected_index)
                .map(|&index| self.flat_nodes[index].node.entry.id().to_string());
            match current_id {
                Some(current_id) if self.folded_nodes.contains(&current_id) => {
                    self.folded_nodes.remove(&current_id);
                    self.apply_filter();
                }
                _ => {
                    self.selected_index = self.find_branch_segment_start("down");
                }
            }
            TreeListOutcome::None
        } else if keybindings::matches(data, action::EDITOR_CURSOR_LEFT)
            || keybindings::matches(data, action::SELECT_PAGE_UP)
        {
            // Page up (`tree-selector.ts:1017-1019`).
            self.selected_index = self.selected_index.saturating_sub(self.max_visible_lines);
            TreeListOutcome::None
        } else if keybindings::matches(data, action::EDITOR_CURSOR_RIGHT)
            || keybindings::matches(data, action::SELECT_PAGE_DOWN)
        {
            // Page down (`tree-selector.ts:1020-1022`).
            self.selected_index = (self.selected_index + self.max_visible_lines)
                .min(self.filtered_nodes.len().saturating_sub(1));
            TreeListOutcome::None
        } else if keybindings::matches(data, action::SELECT_CONFIRM) {
            match self
                .get_selected_node()
                .map(|node| node.entry.id().to_string())
            {
                Some(id) => TreeListOutcome::Selected(id),
                None => TreeListOutcome::None,
            }
        } else if theme::app_keys::matches(data, theme::app_keys::MESSAGE_COPY) {
            TreeListOutcome::Copy(self.copy_selected())
        } else if keybindings::matches(data, action::SELECT_CANCEL) {
            if self.search_query.is_empty() {
                TreeListOutcome::Cancel
            } else {
                // Escape clears the search and the fold state first
                // (`tree-selector.ts:1030-1037`).
                self.search_query.clear();
                self.folded_nodes.clear();
                self.apply_filter();
                TreeListOutcome::None
            }
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_FILTER_DEFAULT) {
            // Direct filter: default (`tree-selector.ts:1038-1042`).
            self.filter_mode = TreeFilterMode::Default;
            self.folded_nodes.clear();
            self.apply_filter();
            TreeListOutcome::None
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_FILTER_NO_TOOLS) {
            // Toggle filter: no-tools ↔ default (`:1043-1047`).
            self.filter_mode = if self.filter_mode == TreeFilterMode::NoTools {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::NoTools
            };
            self.folded_nodes.clear();
            self.apply_filter();
            TreeListOutcome::None
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_FILTER_USER_ONLY) {
            // Toggle filter: user-only ↔ default (`:1048-1052`).
            self.filter_mode = if self.filter_mode == TreeFilterMode::UserOnly {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::UserOnly
            };
            self.folded_nodes.clear();
            self.apply_filter();
            TreeListOutcome::None
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_FILTER_LABELED_ONLY) {
            // Toggle filter: labeled-only ↔ default (`:1053-1057`).
            self.filter_mode = if self.filter_mode == TreeFilterMode::LabeledOnly {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::LabeledOnly
            };
            self.folded_nodes.clear();
            self.apply_filter();
            TreeListOutcome::None
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_FILTER_ALL) {
            // Toggle filter: all ↔ default (`:1058-1062`).
            self.filter_mode = if self.filter_mode == TreeFilterMode::All {
                TreeFilterMode::Default
            } else {
                TreeFilterMode::All
            };
            self.folded_nodes.clear();
            self.apply_filter();
            TreeListOutcome::None
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_FILTER_CYCLE_BACKWARD) {
            // Cycle filter backwards (`:1063-1069`).
            let modes = TreeFilterMode::MODES;
            let current_index = modes
                .iter()
                .position(|mode| *mode == self.filter_mode)
                .unwrap_or(0);
            self.filter_mode = modes[(current_index + modes.len() - 1) % modes.len()];
            self.folded_nodes.clear();
            self.apply_filter();
            TreeListOutcome::None
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_FILTER_CYCLE_FORWARD) {
            // Cycle filter forwards (`:1070-1076`).
            let modes = TreeFilterMode::MODES;
            let current_index = modes
                .iter()
                .position(|mode| *mode == self.filter_mode)
                .unwrap_or(0);
            self.filter_mode = modes[(current_index + 1) % modes.len()];
            self.folded_nodes.clear();
            self.apply_filter();
            TreeListOutcome::None
        } else if keybindings::matches(data, action::EDITOR_DELETE_CHAR_BACKWARD) {
            if !self.search_query.is_empty() {
                self.search_query.pop();
                self.folded_nodes.clear();
                self.apply_filter();
            }
            TreeListOutcome::None
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_EDIT_LABEL) {
            // `app.tree.editLabel` (`:1083-1087`): the container opens the
            // label input for the selected entry.
            match self.get_selected_node() {
                Some(node) => {
                    TreeListOutcome::LabelEdit(node.entry.id().to_string(), node.label.clone())
                }
                None => TreeListOutcome::None,
            }
        } else if theme::app_keys::matches(data, theme::app_keys::TREE_TOGGLE_LABEL_TIMESTAMP) {
            self.show_label_timestamps = !self.show_label_timestamps;
            TreeListOutcome::None
        } else {
            // Printable characters append to the search (`:1090-1100`).
            let has_control_chars = data.chars().any(|ch| {
                let code = ch as u32;
                code < 32 || code == 0x7f || (0x80..=0x9f).contains(&code)
            });
            if !has_control_chars && !data.is_empty() {
                self.search_query.push_str(data);
                self.folded_nodes.clear();
                self.apply_filter();
            }
            TreeListOutcome::None
        }
    }
}

/// `extractFullContent` for user content (`tree-selector.ts:894-905`): the
/// text blocks concatenated.
fn extract_user_content(user: &pi_ai::types::UserMessage) -> String {
    extract_user_content_value(&user.content)
}

/// `extractFullContent` for a `UserContent` payload.
fn extract_user_content_value(content: &pi_ai::types::UserContent) -> String {
    match content {
        pi_ai::types::UserContent::Text(text) => text.clone(),
        pi_ai::types::UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                pi_ai::types::InputContent::Text(text) => Some(text.text.clone()),
                pi_ai::types::InputContent::Image(_) => None,
            })
            .collect::<String>(),
    }
}

/// `extractFullContent` for assistant content.
fn extract_assistant_content(content: &[pi_ai::types::AssistantContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            pi_ai::types::AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<String>()
}

/// `extractFullContent` for tool-result content.
fn extract_input_content(content: &[pi_ai::types::InputContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            pi_ai::types::InputContent::Text(text) => Some(text.text.clone()),
            pi_ai::types::InputContent::Image(_) => None,
        })
        .collect::<String>()
}

/// `hasTextContent` for assistant content (`tree-selector.ts:936-947`).
fn has_text_content_assistant(content: &[pi_ai::types::AssistantContent]) -> bool {
    content.iter().any(|block| {
        matches!(block, pi_ai::types::AssistantContent::Text(text) if !text.text.trim().is_empty())
    })
}

/// The `replacement` discriminator of a context edit
/// (`tree-selector.ts:615`, `:848`).
fn replacement_label(replacement: &crate::session::records::ContextReplacement) -> &'static str {
    match replacement {
        crate::session::records::ContextReplacement::Omit => "omit",
        crate::session::records::ContextReplacement::Replace { .. } => "replace",
    }
}

/// `formatToolCall` (`tree-selector.ts:949-993`).
fn format_tool_call(name: &str, arguments: &serde_json::Map<String, serde_json::Value>) -> String {
    TreeList::format_tool_call(name, arguments)
}

/// `formatLabelTimestamp` (`tree-selector.ts:865-888`): local time; `HH:MM`
/// today, `M/D HH:MM` this year, `YY/M/D HH:MM` otherwise.
fn format_label_timestamp(timestamp: &str) -> String {
    let ms = iso_to_ms(timestamp);
    let Some(time) = DateTime::<Utc>::from_timestamp(
        ms.div_euclid(1000),
        (ms.rem_euclid(1000)) as u32 * 1_000_000,
    ) else {
        return String::new();
    };
    let time = time.with_timezone(&Local);
    let now = Local::now();
    let time_of_day = time.format("%H:%M").to_string();

    if time.year() == now.year() && time.month() == now.month() && time.day() == now.day() {
        return time_of_day;
    }
    // The month and day stay unpadded, like JS `getMonth() + 1`/`getDate()`.
    if time.year() == now.year() {
        return format!("{}/{} {}", time.month(), time.day(), time_of_day);
    }
    let year = (time.year() % 100).to_string();
    format!("{year}/{}/{} {}", time.month(), time.day(), time_of_day)
}

/// One help row (`TREE_HELP_ITEMS`, `tree-selector.ts:1222-1241`).
struct TreeHelpItem {
    actions: &'static [&'static str],
    label: &'static str,
    label_first: bool,
}

/// `TREE_HELP_ITEMS` (`tree-selector.ts:1222-1241`).
const TREE_HELP_ITEMS: &[TreeHelpItem] = &[
    TreeHelpItem {
        actions: &[action::SELECT_UP, action::SELECT_DOWN],
        label: "move",
        label_first: false,
    },
    TreeHelpItem {
        actions: &[action::EDITOR_CURSOR_LEFT, action::EDITOR_CURSOR_RIGHT],
        label: "page",
        label_first: false,
    },
    TreeHelpItem {
        actions: &[
            theme::app_keys::TREE_FOLD_OR_UP,
            theme::app_keys::TREE_UNFOLD_OR_DOWN,
        ],
        label: "branch",
        label_first: false,
    },
    TreeHelpItem {
        actions: &[theme::app_keys::MESSAGE_COPY],
        label: "copy",
        label_first: false,
    },
    TreeHelpItem {
        actions: &[theme::app_keys::TREE_EDIT_LABEL],
        label: "label",
        label_first: false,
    },
    TreeHelpItem {
        actions: &[theme::app_keys::TREE_TOGGLE_LABEL_TIMESTAMP],
        label: "label time",
        label_first: false,
    },
    TreeHelpItem {
        actions: &[
            theme::app_keys::TREE_FILTER_DEFAULT,
            theme::app_keys::TREE_FILTER_NO_TOOLS,
            theme::app_keys::TREE_FILTER_USER_ONLY,
            theme::app_keys::TREE_FILTER_LABELED_ONLY,
            theme::app_keys::TREE_FILTER_ALL,
        ],
        label: "filters",
        label_first: true,
    },
    TreeHelpItem {
        actions: &[
            theme::app_keys::TREE_FILTER_CYCLE_FORWARD,
            theme::app_keys::TREE_FILTER_CYCLE_BACKWARD,
        ],
        label: "cycle",
        label_first: true,
    },
];

/// `formatHelpKeys` (`tree-selector.ts:1243-1258`): the first bound key of
/// each action, compacted, with pi's arrow abbreviations.
fn format_help_keys(actions: &[&str]) -> String {
    let keys: Vec<String> = actions
        .iter()
        .filter_map(|action_id| {
            let keys = if action_id.starts_with("app.") {
                theme::app_keys::keys_for(action_id)
            } else {
                keybindings::keys_for(action_id)
            };
            keys.first().map(|key| (*key).to_string())
        })
        .collect();
    if keys.is_empty() {
        return String::new();
    }

    compact_raw_keys(&keys)
        .replace("pageUp", "pgup")
        .replace("pageDown", "pgdn")
        .replace("up", "↑")
        .replace("down", "↓")
        .replace("left", "←")
        .replace("right", "→")
}

/// `compactRawKeys` (`tree-selector.ts:1260-1273`): a shared modifier prefix
/// collapses to `mod+a/b`.
fn compact_raw_keys(keys: &[String]) -> String {
    if keys.len() == 1 {
        return keys[0].clone();
    }
    let parts: Vec<(String, String)> = keys
        .iter()
        .map(|key| match key.rfind('+') {
            Some(index) => (key[..=index].to_string(), key[index + 1..].to_string()),
            None => (String::new(), key.clone()),
        })
        .collect();
    let prefix = parts[0].0.clone();
    if !prefix.is_empty() && parts.iter().all(|part| part.0 == prefix) {
        let suffixes: Vec<String> = parts.into_iter().map(|part| part.1).collect();
        format!("{prefix}{}", suffixes.join("/"))
    } else {
        keys.join("/")
    }
}

/// `TreeHelp` (`tree-selector.ts:1182-1220`): semantic help rows with
/// chunk-aware wrapping, all muted.
fn render_tree_help(width: usize) -> Vec<String> {
    let theme = theme::theme();
    let items: Vec<String> = TREE_HELP_ITEMS
        .iter()
        .map(|item| {
            let text = format_help_keys(item.actions);
            if text.is_empty() {
                return item.label.to_string();
            }
            if item.label_first {
                format!("{} {text}", item.label)
            } else {
                format!("{text} {}", item.label)
            }
        })
        .collect();

    let available_width = width.max(1);
    let indent = "  ";
    let separator = " · ";
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = String::new();

    for item in items {
        let candidate = if !current_line.is_empty() {
            format!("{current_line}{separator}{item}")
        } else if visible_width(&format!("{indent}{item}")) <= available_width {
            format!("{indent}{item}")
        } else {
            item.clone()
        };
        if current_line.is_empty() || visible_width(&candidate) <= available_width {
            current_line = candidate;
            continue;
        }

        lines.extend(wrap_text_with_ansi(
            current_line.trim_end(),
            available_width,
        ));
        current_line = if visible_width(&format!("{indent}{item}")) <= available_width {
            format!("{indent}{item}")
        } else {
            item.clone()
        };
    }

    if !current_line.is_empty() {
        lines.extend(wrap_text_with_ansi(
            current_line.trim_end(),
            available_width,
        ));
    }

    lines
        .into_iter()
        .map(|line| theme.fg(ThemeColor::Muted, &line))
        .collect()
}

/// The label input (`LabelInput`, `tree-selector.ts:1276-1328`).
struct LabelInput {
    input: Input,
    entry_id: String,
}

/// What one label-input round produced (`onSubmit`/`onCancel`).
enum LabelInputOutcome {
    None,
    Submit(Option<String>),
    Cancel,
}

impl LabelInput {
    fn new(entry_id: &str, current_label: Option<&str>) -> Self {
        let mut input = Input::new(InputOptions::default());
        if let Some(label) = current_label {
            input.set_value(label);
        }
        Self {
            input,
            entry_id: entry_id.to_string(),
        }
    }

    /// `render` (`tree-selector.ts:1302-1315`).
    fn render(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let indent = "  ";
        let mut lines = Vec::new();
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}",
                theme.fg(ThemeColor::Muted, "Label (empty to remove):")
            ),
            width,
            "",
            false,
        ));
        for line in Component::render(&self.input, width.saturating_sub(indent.len())) {
            lines.push(truncate_to_width(
                &format!("{indent}{line}"),
                width,
                "",
                false,
            ));
        }
        lines.push(truncate_to_width(
            &format!(
                "{indent}{}  {}",
                theme::key_hint(action::SELECT_CONFIRM, "save"),
                theme::key_hint(action::SELECT_CANCEL, "cancel"),
            ),
            width,
            "",
            false,
        ));
        lines
    }

    /// `handleInput` (`tree-selector.ts:1317-1327`).
    fn handle_input(&mut self, data: &str) -> LabelInputOutcome {
        if keybindings::matches(data, action::SELECT_CONFIRM) {
            let value = self.input.value().trim().to_string();
            let label = if value.is_empty() { None } else { Some(value) };
            LabelInputOutcome::Submit(label)
        } else if keybindings::matches(data, action::SELECT_CANCEL) {
            LabelInputOutcome::Cancel
        } else {
            let _ = Component::handle_input(&mut self.input, data);
            LabelInputOutcome::None
        }
    }

    /// The `focused` propagation (`tree-selector.ts:1282-1290`): the inner
    /// input owns the hardware-cursor marker.
    fn set_focused(&mut self, focused: bool) {
        Component::set_focused(&mut self.input, focused);
    }
}

/// Component that renders the session tree selector
/// (`TreeSelectorComponent`, `tree-selector.ts:1333-1438`).
pub struct TreeSelectorComponent {
    tree_list: TreeList,
    label_input: Option<LabelInput>,
    focused: bool,
    events: Arc<Mutex<Vec<TreeSelectorEvent>>>,
}

impl TreeSelectorComponent {
    pub fn new(
        tree: &[SessionTreeNode],
        current_leaf_id: Option<&str>,
        terminal_height: usize,
        initial_selected_id: Option<&str>,
        initial_filter_mode: TreeFilterMode,
        events: Arc<Mutex<Vec<TreeSelectorEvent>>>,
    ) -> Self {
        // `maxVisibleLines = Math.max(5, Math.floor(terminalHeight / 2))`
        // (`tree-selector.ts:1368`).
        let max_visible_lines = (terminal_height / 2).max(5);
        Self {
            tree_list: TreeList::new(
                tree,
                current_leaf_id,
                max_visible_lines,
                initial_selected_id,
                initial_filter_mode,
            ),
            label_input: None,
            focused: false,
            events,
        }
    }

    /// The tree list, for the orchestrator's pre-selection checks
    /// (`getTreeList`, `tree-selector.ts:1436-1438`).
    pub fn tree_list(&self) -> &TreeList {
        &self.tree_list
    }

    /// `showLabelInput` (`tree-selector.ts:1399-1414`).
    fn show_label_input(&mut self, entry_id: &str, current_label: Option<&str>) {
        self.label_input = Some(LabelInput::new(entry_id, current_label));
        let focused = self.focused;
        self.set_focused(focused);
    }

    /// `hideLabelInput` (`tree-selector.ts:1416-1422`).
    fn hide_label_input(&mut self) {
        self.label_input = None;
        let focused = self.focused;
        self.set_focused(focused);
    }

    /// The container's rows: header, help, search line, then either the tree
    /// or the label input (`tree-selector.ts:1382-1392`).
    fn render_base(&self, width: usize) -> Vec<String> {
        let mut lines = vec![String::new()];
        lines.extend(Component::render(&DynamicBorder::new(), width));
        lines.extend(Text::new(theme::bold("  Session Tree"), 1, 0).render(width));
        lines.extend(render_tree_help(width));
        lines.push(self.render_search_line(width));
        lines.extend(Component::render(&DynamicBorder::new(), width));
        lines.push(String::new());
        match &self.label_input {
            Some(label_input) => lines.extend(label_input.render(width)),
            None => lines.extend(self.tree_list.render(width)),
        }
        lines.push(String::new());
        lines.extend(Component::render(&DynamicBorder::new(), width));
        lines
    }

    /// `SearchLine.render` (`tree-selector.ts:1166-1177`): the query, plus
    /// the inverse-video cursor while focused and no label input is open.
    fn render_search_line(&self, width: usize) -> String {
        let theme = theme::theme();
        let query = &self.tree_list.search_query;
        let line = if query.is_empty() {
            format!("  {} ", theme.fg(ThemeColor::Muted, "Type to search:"))
        } else {
            format!(
                "  {} {}",
                theme.fg(ThemeColor::Muted, "Type to search:"),
                theme.fg(ThemeColor::Accent, query)
            )
        };
        if !self.focused || self.label_input.is_some() {
            return truncate_to_width(&line, width, "", false);
        }
        let content = truncate_to_width(&line, width.saturating_sub(1), "", false);
        format!("{content}{CURSOR_MARKER}\x1b[7m \x1b[27m")
    }
}

impl Component for TreeSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_base(width)
    }

    /// `handleInput` (`tree-selector.ts:1428-1434`): the label input owns the
    /// keys while open; the tree list's outcomes map onto the callbacks
    /// (`onSelect`/`onCancel`/`onCopy` ride the queue, `onLabelEdit` opens
    /// the internal input).
    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        if let Some(label_input) = &mut self.label_input {
            match label_input.handle_input(data) {
                LabelInputOutcome::Submit(label) => {
                    let entry_id = label_input.entry_id.clone();
                    // `treeList.updateNodeLabel(id, label)` then
                    // `onLabelChangeCallback?.(id, label)` (`:1401-1404`).
                    self.tree_list
                        .update_node_label(&entry_id, label.as_deref());
                    self.events
                        .lock()
                        .expect("tree selector events")
                        .push(TreeSelectorEvent::LabelChange(entry_id, label));
                    self.hide_label_input();
                }
                LabelInputOutcome::Cancel => self.hide_label_input(),
                LabelInputOutcome::None => {}
            }
        } else {
            match self.tree_list.handle_input(data) {
                TreeListOutcome::None => {}
                TreeListOutcome::Selected(id) => {
                    self.events
                        .lock()
                        .expect("tree selector events")
                        .push(TreeSelectorEvent::Selected(id));
                }
                TreeListOutcome::Cancel => {
                    self.events
                        .lock()
                        .expect("tree selector events")
                        .push(TreeSelectorEvent::Cancel);
                }
                TreeListOutcome::Copy(text) => {
                    self.events
                        .lock()
                        .expect("tree selector events")
                        .push(TreeSelectorEvent::Copy(text));
                }
                TreeListOutcome::LabelEdit(entry_id, label) => {
                    self.show_label_input(&entry_id, label.as_deref());
                }
            }
        }
        Some(ComponentMutation { consume: true })
    }

    fn has_input_handler(&self) -> bool {
        true
    }

    /// The `focused` setter (`tree-selector.ts:1347-1353`): the search line
    /// keeps the cursor unless the label input is open; the label input
    /// propagates to its inner editor.
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if let Some(label_input) = &mut self.label_input {
            label_input.set_focused(focused);
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

    // The upstream test's escape sequences (`tree-selector.test.ts:570-575`).
    const DOWN: &str = "\x1b[B";
    const CTRL_LEFT: &str = "\x1b[1;5D";
    const CTRL_RIGHT: &str = "\x1b[1;5C";

    /// A user message entry (`userMessage`, `tree-selector.test.ts:29-37`).
    fn user_message(id: &str, parent_id: Option<&str>, content: &str) -> SessionEntry {
        SessionEntry::Message(crate::session::records::SessionMessageEntry {
            base: crate::session::records::EntryBase {
                id: id.to_string(),
                parent_id: parent_id.map(str::to_string),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            },
            message: pi_agent::AgentMessage::User(pi_ai::types::UserMessage {
                content: pi_ai::types::UserContent::Text(content.to_string()),
                timestamp: 0,
            }),
            unknown: serde_json::Map::new(),
        })
    }

    /// An assistant text message
    /// (`assistantMessage`, `tree-selector.test.ts:39-64`).
    fn assistant_message(id: &str, parent_id: Option<&str>, text: &str) -> SessionEntry {
        let message: pi_ai::types::AssistantMessage = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
            "api": "anthropic-messages",
            "provider": "anthropic",
            "model": "claude-sonnet-4",
            "usage": {
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 0,
                "cost": { "input": 0, "output": 0, "cacheRead": 0,
                          "cacheWrite": 0, "total": 0 }
            },
            "stopReason": "stop",
            "timestamp": 0
        }))
        .expect("assistant message");
        SessionEntry::Message(crate::session::records::SessionMessageEntry {
            base: crate::session::records::EntryBase {
                id: id.to_string(),
                parent_id: parent_id.map(str::to_string),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            },
            message: pi_agent::AgentMessage::Assistant(message),
            unknown: serde_json::Map::new(),
        })
    }

    /// A tool-call-only assistant message
    /// (`toolCallOnlyAssistant`, `tree-selector.test.ts:66-91`).
    fn tool_call_only_assistant(id: &str, parent_id: Option<&str>) -> SessionEntry {
        let message: pi_ai::types::AssistantMessage = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "toolCall", "id": format!("tc-{id}"), "name": "read",
                          "arguments": { "path": "test.ts" } }],
            "api": "anthropic-messages",
            "provider": "anthropic",
            "model": "claude-sonnet-4",
            "usage": {
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 0,
                "cost": { "input": 0, "output": 0, "cacheRead": 0,
                          "cacheWrite": 0, "total": 0 }
            },
            "stopReason": "toolUse",
            "timestamp": 0
        }))
        .expect("assistant message");
        SessionEntry::Message(crate::session::records::SessionMessageEntry {
            base: crate::session::records::EntryBase {
                id: id.to_string(),
                parent_id: parent_id.map(str::to_string),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            },
            message: pi_agent::AgentMessage::Assistant(message),
            unknown: serde_json::Map::new(),
        })
    }

    /// A `model_change` entry (`modelChange`, `tree-selector.test.ts:93-101`).
    fn model_change(id: &str, parent_id: Option<&str>) -> SessionEntry {
        SessionEntry::ModelChange(crate::session::records::ModelChangeEntry {
            base: crate::session::records::EntryBase {
                id: id.to_string(),
                parent_id: parent_id.map(str::to_string),
                timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            },
            provider: "anthropic".to_string(),
            model_id: "claude-sonnet-4".to_string(),
            unknown: serde_json::Map::new(),
        })
    }

    /// Build the tree from parentId relationships
    /// (`buildTree`, `tree-selector.test.ts:103-131`). Entries are in
    /// chronological order, so a depth-first assembly over the index map
    /// reproduces upstream's shared-node structure.
    fn build_tree(entries: Vec<SessionEntry>) -> Vec<SessionTreeNode> {
        /// Depth-first assembly of the linked tree (the upstream test shares
        /// node references; the port clones the subtree instead).
        fn assemble(
            nodes: &[SessionTreeNode],
            children_of: &[Vec<usize>],
            index: usize,
        ) -> SessionTreeNode {
            let mut node = nodes[index].clone();
            node.children = children_of[index]
                .iter()
                .map(|&child| assemble(nodes, children_of, child))
                .collect();
            node
        }

        if entries.is_empty() {
            return Vec::new();
        }
        let nodes: Vec<SessionTreeNode> = entries
            .into_iter()
            .map(|entry| SessionTreeNode {
                entry,
                children: Vec::new(),
                label: None,
                label_timestamp: None,
            })
            .collect();
        let mut by_id: HashMap<String, usize> = HashMap::new();
        for (index, node) in nodes.iter().enumerate() {
            by_id.insert(node.entry.id().to_string(), index);
        }
        let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        let mut roots: Vec<usize> = Vec::new();
        for (index, node) in nodes.iter().enumerate() {
            match node.entry.parent_id().map(str::to_string) {
                None => roots.push(index),
                Some(parent) => match by_id.get(&parent).copied() {
                    Some(parent_index) => children_of[parent_index].push(index),
                    None => roots.push(index),
                },
            }
        }

        roots
            .into_iter()
            .map(|index| assemble(&nodes, &children_of, index))
            .collect()
    }

    /// A selector over an events queue (the default theme auto-initializes).
    fn selector_for(tree: &[SessionTreeNode], leaf: &str) -> TreeSelectorComponent {
        TreeSelectorComponent::new(
            tree,
            Some(leaf),
            24,
            None,
            TreeFilterMode::Default,
            Arc::new(Mutex::new(Vec::new())),
        )
    }

    fn selected_id(selector: &TreeSelectorComponent) -> String {
        selector
            .tree_list()
            .get_selected_node()
            .map(|node| node.entry.id().to_string())
            .unwrap_or_default()
    }

    fn take_events(selector: &TreeSelectorComponent) -> Vec<TreeSelectorEvent> {
        std::mem::take(&mut *selector.events.lock().expect("events"))
    }

    /// `focuses nearest visible ancestor when currentLeafId is a
    /// model_change with sibling branch` (`tree-selector.test.ts:136-157`).
    #[test]
    fn focuses_nearest_visible_ancestor_when_leaf_is_metadata() {
        let entries = vec![
            user_message("user-1", None, "hello"),
            assistant_message("asst-1", Some("user-1"), "hi"),
            user_message("user-2", Some("asst-1"), "active branch"),
            model_change("model-1", Some("user-2")),
            user_message("user-3", Some("asst-1"), "sibling branch"),
        ];
        let tree = build_tree(entries);
        let selector = selector_for(&tree, "model-1");
        // The metadata leaf is hidden; the walk lands on its parent user-2,
        // not on the chronologically later user-3.
        assert_eq!(selected_id(&selector), "user-2");
    }

    /// `switches to nearest visible user message when changing to user-only
    /// filter` + the switch back (`tree-selector.test.ts:220-269`).
    #[test]
    fn user_only_filter_walks_to_the_parent_user_message() {
        let entries = vec![
            user_message("user-1", None, "hello"),
            assistant_message("asst-1", Some("user-1"), "hi"),
            user_message("user-2", Some("asst-1"), "active branch"),
            assistant_message("asst-2", Some("user-2"), "response"),
            user_message("user-3", Some("asst-1"), "sibling branch"),
        ];
        let tree = build_tree(entries);
        let mut selector = selector_for(&tree, "asst-2");
        assert_eq!(selected_id(&selector), "asst-2");

        Component::handle_input(&mut selector, "\x15"); // ctrl+u
        assert_eq!(selected_id(&selector), "user-2");

        Component::handle_input(&mut selector, "\x04"); // ctrl+d
        assert_eq!(selected_id(&selector), "user-2");
    }

    /// `renders semantic help rows without truncating narrow terminal
    /// controls` (`tree-selector.test.ts:272-295`).
    #[test]
    fn help_renders_semantic_rows_within_the_width() {
        let tree = build_tree(vec![
            user_message("user-1", None, "hello"),
            assistant_message("asst-1", Some("user-1"), "hi"),
        ]);
        let selector = selector_for(&tree, "asst-1");
        let lines = Component::render(&selector, 30);
        let plain_lines: Vec<String> = lines
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect();
        let plain = plain_lines.join("\n");
        assert!(plain.contains("branch"), "{plain}");
        assert!(plain.contains("copy"), "{plain}");
        assert!(plain.contains("filters"), "{plain}");
        assert!(plain.contains("cycle"), "{plain}");
        assert!(plain.contains("label time"), "{plain}");
        assert!(!plain.contains("..."), "{plain}");
        assert!(
            plain_lines.iter().all(|line| visible_width(line) <= 30),
            "{plain_lines:?}"
        );
    }

    /// `renders the cursor only while the selector is focused`
    /// (`tree-selector.test.ts:298-315`).
    #[test]
    fn search_cursor_only_while_focused() {
        let tree = build_tree(vec![user_message("user-1", None, "hello")]);
        let mut selector = selector_for(&tree, "user-1");

        let lines = Component::render(&selector, 80);
        assert!(
            lines.iter().all(|line| !line.contains(CURSOR_MARKER)),
            "{lines:?}"
        );

        selector.set_focused(true);
        let lines = Component::render(&selector, 80);
        let cursor_lines: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains(CURSOR_MARKER))
            .collect();
        assert_eq!(cursor_lines.len(), 1);
        assert_eq!(
            strip_terminal_sequences(&cursor_lines[0].replace(CURSOR_MARKER, "")),
            "  Type to search:  "
        );

        selector.set_focused(false);
        let lines = Component::render(&selector, 80);
        assert!(
            lines.iter().all(|line| !line.contains(CURSOR_MARKER)),
            "{lines:?}"
        );
    }

    /// `copies the full selected message with ctrl+x`
    /// (`tree-selector.test.ts:393-415`).
    #[test]
    fn copy_emits_the_full_message_text() {
        let message = format!("{}\nsecond line", "long message ".repeat(30));
        let tree = build_tree(vec![
            user_message("user-1", None, "hello"),
            assistant_message("asst-1", Some("user-1"), &message),
        ]);
        let mut selector = selector_for(&tree, "asst-1");

        Component::handle_input(&mut selector, "\x18"); // ctrl+x
        let events = take_events(&selector);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            matches!(&events[0], TreeSelectorEvent::Copy(Some(text)) if *text == message),
            "{events:?}"
        );
    }

    /// `toggles label timestamps for labeled nodes`
    /// (`tree-selector.test.ts:418-434`), driven through shift+t.
    #[test]
    fn label_timestamps_toggle_with_shift_t() {
        use chrono::TimeZone;
        let mut tree = build_tree(vec![
            user_message("user-1", None, "hello"),
            assistant_message("asst-1", Some("user-1"), "hi"),
        ]);
        let timestamp = chrono::Local
            .with_ymd_and_hms(2026, 3, 28, 14, 32, 0)
            .single()
            .expect("local datetime")
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, false);
        tree[0].label = Some("checkpoint".to_string());
        tree[0].label_timestamp = Some(timestamp.clone());

        let mut selector = selector_for(&tree, "asst-1");
        let render = strip_terminal_sequences(&Component::render(&selector, 200).join("\n"));
        assert!(render.contains("[checkpoint]"), "{render}");
        assert!(!render.contains("[+label time]"), "{render}");

        Component::handle_input(&mut selector, "T"); // shift+t

        let render = strip_terminal_sequences(&Component::render(&selector, 200).join("\n"));
        assert!(render.contains("[+label time]"), "{render}");
        assert!(
            render.contains(&format_label_timestamp(&timestamp)),
            "{render}"
        );
    }

    /// `preserves selection when switching to empty labeled filter and back`
    /// (`tree-selector.test.ts:437-478`).
    #[test]
    fn empty_labeled_filter_preserves_the_selection() {
        let entries = vec![
            user_message("user-1", None, "hello"),
            assistant_message("asst-1", Some("user-1"), "hi"),
            user_message("user-2", Some("asst-1"), "bye"),
            assistant_message("asst-2", Some("user-2"), "goodbye"),
        ];
        let tree = build_tree(entries);
        let mut selector = selector_for(&tree, "asst-2");
        assert_eq!(selected_id(&selector), "asst-2");

        Component::handle_input(&mut selector, "\x0c"); // ctrl+l: labeled-only
        assert!(selector.tree_list().get_selected_node().is_none());

        Component::handle_input(&mut selector, "\x04"); // ctrl+d: default
        assert_eq!(selected_id(&selector), "asst-2");
    }

    /// `preserves selection through multiple empty filter switches`
    /// (`tree-selector.test.ts:480-507`).
    #[test]
    fn selection_survives_repeated_empty_filter_switches() {
        let tree = build_tree(vec![
            user_message("user-1", None, "hello"),
            assistant_message("asst-1", Some("user-1"), "hi"),
        ]);
        let mut selector = selector_for(&tree, "asst-1");
        assert_eq!(selected_id(&selector), "asst-1");

        Component::handle_input(&mut selector, "\x0c"); // ctrl+l -> labeled-only
        assert!(selector.tree_list().get_selected_node().is_none());

        Component::handle_input(&mut selector, "\x0c"); // ctrl+l -> default
        assert_eq!(selected_id(&selector), "asst-1");

        Component::handle_input(&mut selector, "\x0c"); // ctrl+l -> labeled-only
        assert!(selector.tree_list().get_selected_node().is_none());

        Component::handle_input(&mut selector, "\x04"); // ctrl+d -> default
        assert_eq!(selected_id(&selector), "asst-1");
    }

    /// The branch sequence from `ctrl+right unfolds a folded node, then does
    /// segment jump when unfolded` (`tree-selector.test.ts:597-627`).
    #[test]
    fn ctrl_arrows_fold_unfold_and_jump_segments() {
        // `buildBranchingTree` (`tree-selector.test.ts:578-596`).
        let tree = build_tree(vec![
            user_message("user-1", None, "first message"),
            assistant_message("asst-1", Some("user-1"), "response 1"),
            user_message("user-2", Some("asst-1"), "second message"),
            assistant_message("asst-2", Some("user-2"), "response 2"),
            user_message("user-3a", Some("asst-2"), "branch A start"),
            assistant_message("asst-3a", Some("user-3a"), "branch A response"),
            user_message("user-4a", Some("asst-3a"), "branch A deep"),
            assistant_message("asst-4a", Some("user-4a"), "branch A leaf"),
            user_message("user-3b", Some("asst-2"), "branch B start"),
            assistant_message("asst-3b", Some("user-3b"), "branch B response"),
            user_message("user-4b", Some("asst-3b"), "branch B deep"),
        ]);
        let mut selector = selector_for(&tree, "asst-4a");

        Component::handle_input(&mut selector, CTRL_LEFT); // asst-4a → user-3a
        assert_eq!(selected_id(&selector), "user-3a");

        Component::handle_input(&mut selector, CTRL_LEFT); // fold user-3a
        assert_eq!(selected_id(&selector), "user-3a");

        Component::handle_input(&mut selector, DOWN); // user-3a → user-3b
        assert_eq!(selected_id(&selector), "user-3b");

        Component::handle_input(&mut selector, "\x1b[A"); // up: user-3b → user-3a
        assert_eq!(selected_id(&selector), "user-3a");

        Component::handle_input(&mut selector, CTRL_RIGHT); // unfold user-3a
        assert_eq!(selected_id(&selector), "user-3a");

        Component::handle_input(&mut selector, DOWN); // user-3a → asst-3a
        assert_eq!(selected_id(&selector), "asst-3a");

        Component::handle_input(&mut selector, CTRL_LEFT); // asst-3a → user-3a
        assert_eq!(selected_id(&selector), "user-3a");

        Component::handle_input(&mut selector, CTRL_RIGHT); // segment jump to leaf
        assert_eq!(selected_id(&selector), "asst-4a");
    }

    /// `folding root hides entire subtree, nested fold preserved on unfold`
    /// (`tree-selector.test.ts:657-684`).
    #[test]
    fn folding_the_root_hides_the_subtree() {
        let tree = build_tree(vec![
            user_message("user-1", None, "first message"),
            assistant_message("asst-1", Some("user-1"), "response 1"),
            user_message("user-2", Some("asst-1"), "second message"),
            assistant_message("asst-2", Some("user-2"), "response 2"),
            user_message("user-3a", Some("asst-2"), "branch A start"),
            assistant_message("asst-3a", Some("user-3a"), "branch A response"),
            user_message("user-4a", Some("asst-3a"), "branch A deep"),
            assistant_message("asst-4a", Some("user-4a"), "branch A leaf"),
            user_message("user-3b", Some("asst-2"), "branch B start"),
            assistant_message("asst-3b", Some("user-3b"), "branch B response"),
            user_message("user-4b", Some("asst-3b"), "branch B deep"),
        ]);
        let mut selector = selector_for(&tree, "asst-4a");

        Component::handle_input(&mut selector, CTRL_LEFT); // → user-3a
        Component::handle_input(&mut selector, CTRL_LEFT); // fold user-3a
        assert_eq!(selected_id(&selector), "user-3a");

        Component::handle_input(&mut selector, CTRL_LEFT); // user-3a (folded) → user-1
        assert_eq!(selected_id(&selector), "user-1");

        Component::handle_input(&mut selector, CTRL_LEFT); // fold user-1
        assert_eq!(selected_id(&selector), "user-1");

        Component::handle_input(&mut selector, DOWN); // wrap (only visible node)
        assert_eq!(selected_id(&selector), "user-1");

        Component::handle_input(&mut selector, CTRL_RIGHT); // unfold user-1
        assert_eq!(selected_id(&selector), "user-1");

        Component::handle_input(&mut selector, CTRL_RIGHT); // → user-3a (still folded)
        assert_eq!(selected_id(&selector), "user-3a");

        Component::handle_input(&mut selector, DOWN); // user-3a → user-3b
        assert_eq!(selected_id(&selector), "user-3b");
    }

    /// `search resets fold state` (`tree-selector.test.ts:713-737`).
    #[test]
    fn typing_a_search_resets_the_fold_state() {
        let tree = build_tree(vec![
            user_message("user-1", None, "first message"),
            assistant_message("asst-1", Some("user-1"), "response 1"),
            user_message("user-2", Some("asst-1"), "second message"),
            assistant_message("asst-2", Some("user-2"), "response 2"),
            user_message("user-3a", Some("asst-2"), "branch A start"),
            assistant_message("asst-3a", Some("user-3a"), "branch A response"),
            user_message("user-4a", Some("asst-3a"), "branch A deep"),
            assistant_message("asst-4a", Some("user-4a"), "branch A leaf"),
            user_message("user-3b", Some("asst-2"), "branch B start"),
            assistant_message("asst-3b", Some("user-3b"), "branch B response"),
            user_message("user-4b", Some("asst-3b"), "branch B deep"),
        ]);
        let mut selector = selector_for(&tree, "asst-4a");

        Component::handle_input(&mut selector, CTRL_LEFT); // → user-3a
        Component::handle_input(&mut selector, CTRL_LEFT); // fold user-3a
        Component::handle_input(&mut selector, DOWN); // → user-3b
        assert_eq!(selected_id(&selector), "user-3b");

        Component::handle_input(&mut selector, "b"); // search resets folds
        Component::handle_input(&mut selector, "\x1b"); // clear search

        let mut current_id = String::new();
        for _ in 0..20 {
            Component::handle_input(&mut selector, DOWN);
            current_id = selected_id(&selector);
            if current_id == "user-3a" {
                break;
            }
        }
        assert_eq!(current_id, "user-3a");

        Component::handle_input(&mut selector, DOWN); // user-3a → asst-3a
        assert_eq!(selected_id(&selector), "asst-3a");
    }

    /// `filter mode change resets fold state` (`tree-selector.test.ts:739-765`).
    #[test]
    fn switching_filters_resets_the_fold_state() {
        let tree = build_tree(vec![
            user_message("user-1", None, "first message"),
            assistant_message("asst-1", Some("user-1"), "response 1"),
            user_message("user-2", Some("asst-1"), "second message"),
            assistant_message("asst-2", Some("user-2"), "response 2"),
            user_message("user-3a", Some("asst-2"), "branch A start"),
            assistant_message("asst-3a", Some("user-3a"), "branch A response"),
            user_message("user-4a", Some("asst-3a"), "branch A deep"),
            assistant_message("asst-4a", Some("user-4a"), "branch A leaf"),
            user_message("user-3b", Some("asst-2"), "branch B start"),
            assistant_message("asst-3b", Some("user-3b"), "branch B response"),
            user_message("user-4b", Some("asst-3b"), "branch B deep"),
        ]);
        let mut selector = selector_for(&tree, "asst-4a");

        Component::handle_input(&mut selector, CTRL_LEFT); // → user-3a
        Component::handle_input(&mut selector, CTRL_LEFT); // fold user-3a

        Component::handle_input(&mut selector, "\x15"); // ctrl+u: user-only
        Component::handle_input(&mut selector, "\x04"); // ctrl+d: default

        let mut current_id = String::new();
        for _ in 0..20 {
            Component::handle_input(&mut selector, DOWN);
            current_id = selected_id(&selector);
            if current_id == "user-3a" {
                break;
            }
        }
        assert_eq!(current_id, "user-3a");

        Component::handle_input(&mut selector, DOWN); // user-3a → asst-3a
        assert_eq!(selected_id(&selector), "asst-3a");
    }

    /// The label input opens on shift+l, and enter commits the label
    /// (`moves focus to the label input and restores it after cancellation`,
    /// `tree-selector.test.ts:318-335`, plus the `showLabelInput` submit flow).
    #[test]
    fn label_edit_opens_and_submits() {
        let tree = build_tree(vec![user_message("user-1", None, "hello")]);
        let mut selector = selector_for(&tree, "user-1");
        selector.set_focused(true);

        Component::handle_input(&mut selector, "L"); // shift+l opens the input
        let rendered = Component::render(&selector, 80);
        let cursor_index = rendered
            .iter()
            .position(|line| line.contains(CURSOR_MARKER))
            .expect("cursor line");
        assert!(
            strip_terminal_sequences(&rendered[cursor_index - 1])
                .contains("Label (empty to remove):"),
            "{rendered:?}"
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains(CURSOR_MARKER))
                .count(),
            1
        );

        Component::handle_input(&mut selector, "checkpoint");
        Component::handle_input(&mut selector, "\r"); // enter saves
        let events = take_events(&selector);
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            matches!(&events[0],
                TreeSelectorEvent::LabelChange(entry_id, Some(label))
                    if entry_id == "user-1" && label == "checkpoint"),
            "{events:?}"
        );
        // The label shows in the tree row after the edit.
        let plain = strip_terminal_sequences(&Component::render(&selector, 200).join("\n"));
        assert!(plain.contains("[checkpoint]"), "{plain}");
        // The cursor moved back to the search line.
        let rendered = Component::render(&selector, 80);
        let cursor_index = rendered
            .iter()
            .position(|line| line.contains(CURSOR_MARKER))
            .expect("cursor line");
        assert!(
            strip_terminal_sequences(&rendered[cursor_index]).contains("Type to search:"),
            "{rendered:?}"
        );
    }

    /// Escape inside the label input closes it without saving
    /// (`onCancel` → `hideLabelInput`).
    #[test]
    fn label_edit_escape_closes_without_saving() {
        let tree = build_tree(vec![user_message("user-1", None, "hello")]);
        let mut selector = selector_for(&tree, "user-1");

        Component::handle_input(&mut selector, "L"); // shift+l
        Component::handle_input(&mut selector, "\x1b"); // escape

        assert!(take_events(&selector).is_empty(), "no label change");
        let plain = strip_terminal_sequences(&Component::render(&selector, 200).join("\n"));
        assert!(!plain.contains("Label (empty to remove):"), "{plain}");
        assert!(plain.contains("Type to search:"), "{plain}");
    }

    /// `folding root hides descendants even when intermediate nodes are
    /// filtered out` (`tree-selector.test.ts:686-712`).
    #[test]
    fn folding_the_root_hides_descendants_past_filtered_nodes() {
        // user-1 -> toolCallOnly-1 (filtered out) -> user-2 -> asst-2
        let tree = build_tree(vec![
            user_message("user-1", None, "hello"),
            tool_call_only_assistant("tool-asst-1", Some("user-1")),
            user_message("user-2", Some("tool-asst-1"), "follow up"),
            assistant_message("asst-2", Some("user-2"), "response"),
        ]);
        let mut selector = selector_for(&tree, "asst-2");

        Component::handle_input(&mut selector, CTRL_LEFT); // asst-2 -> user-1
        assert_eq!(selected_id(&selector), "user-1");

        Component::handle_input(&mut selector, CTRL_LEFT); // fold user-1
        assert_eq!(selected_id(&selector), "user-1");

        Component::handle_input(&mut selector, DOWN); // wrap (only visible node)
        assert_eq!(selected_id(&selector), "user-1");
    }
}
