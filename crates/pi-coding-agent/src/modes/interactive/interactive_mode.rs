//! The interactive mode orchestrator: the chat TUI over the agent loop.
//!
//! Ground truth: `modes/interactive/interactive-mode.ts`. This port covers the
//! regular (main-screen) chat loop — the startup header, user and assistant
//! messages, tool execution rows, the working status embedded in the editor's
//! top border, the footer, the `/`-commands this phase wires, the `!` bash
//! mode, the editor autocomplete (slash commands, `@` files, paths), context
//! compaction, and the OSC theme controller. Fullscreen mode is ported as the
//! alt-screen renderer choice plus the chat viewport ([`chat_viewport`])
//! mounted over the same component tree (`switchTuiMode`,
//! `interactive-mode.ts:641-700`); the selector dialogs are editor-slot
//! swaps, and the extension host is deferred.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_agent::agent_loop::{AgentLoopConfig, DefaultConvertToLlm};
use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEvent, AgentEventSink};
use pi_agent::queues::Queues;
use pi_agent::transcript::tool_declaration;
use pi_agent::types::{
    AbortSignal, AgentContext, QueueMode, ThinkingLevel, ToolExecutionMode, ToolExecutor,
};
use pi_tui::color::{TextAttributes, style_text_with_ansi};
use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::editor::{Editor, EditorOptions};
use pi_tui::components::scroll_view::ScrollView;
use pi_tui::components::stack::VStack;
use pi_tui::components::text::Text;
use pi_tui::mouse::{ComponentId, MouseEvent, MouseEventResult, component_id};
use pi_tui::stdin_buffer::{StdinBuffer, StdinBufferOptions, StdinEvent};
use pi_tui::terminal::CrosstermTerminal;
use pi_tui::terminal_colors::TerminalColorScheme;
use pi_tui::theme::ThemeAppearance;
use pi_tui::tui::FocusTarget;
use pi_tui::tui::{TerminalColors, Tui, TuiStopOptions};
use pi_tui::tui_alt_screen::AltScreenOptions;

use crate::cli::args::TuiMode;
use crate::cli::project_trust::ProjectTrustStore;
use crate::config::settings::{
    FullscreenExitOutput, FullscreenScrollbar, SettingsManager, SettingsManagerCreateOptions,
};
use crate::modes::print_mode::PrintRuntime;
use crate::session::store::{NewSessionOptions, SessionManager};

use super::autocomplete;
use super::chat_viewport::{self, ChatViewport, HostSlot};
use super::components::armin::ArminComponent;
use super::components::assistant_message::AssistantMessageComponent;
use super::components::auth_selector::{
    AuthSelectorComponent, AuthSelectorEvent, LoginPromptComponent, LoginPromptEvent,
};
use super::components::easter_egg_3d::{EasterEgg3d, EasterEggHandle, play_easter_egg_3d};
use super::components::footer::{FooterComponent, FooterData, UsageTotals};
use super::components::header::{BuiltInHeader, pi_logo_lines};
use super::components::model_selector::{ModelSelectorComponent, ModelSelectorEvent};
use super::components::session_selector::{SessionSelectorComponent, SessionSelectorEvent};
use super::components::settings_selector::{
    SettingsSelectorComponent, SettingsSelectorConfig, SettingsSelectorEvent,
};
use super::components::status_indicator::{IdleStatus, StatusIndicator};
use super::components::tool_execution::{ToolExecutionComponent, ToolResultView};
use super::components::trust_selector::{TrustSelectorComponent, TrustSelectorEvent};
use super::components::user_message::UserMessageComponent;
use super::theme;
use crate::oauth::{self, AuthSelectorProvider};

/// Where a cancelled login returns to (`startProviderLogin`'s `onBack`,
/// `interactive-mode.ts:6077-6085`).
#[derive(Debug, Clone)]
enum LoginBackStep {
    /// `showLoginAuthTypeSelector(providerOptions)` again.
    AuthType(Option<Vec<AuthSelectorProvider>>),
    /// `showLoginProviderSelector(authType)` again.
    Provider(Option<&'static str>),
}

/// Options the CLI host passes in (`InteractiveModeOptions`).
pub struct InteractiveOptions {
    pub version: String,
    /// A `--theme` override; the settings getter lands with the settings work.
    pub theme_name: Option<String>,
    /// A `--tui-mode` override; unset resolves through the settings
    /// (`getTuiMode`, which defaults to fullscreen).
    pub tui_mode: Option<TuiMode>,
    /// The `auth.json` path for the model-availability check. `None` uses the
    /// real agent dir; tests inject a temp path to stay hermetic.
    pub auth_path: Option<std::path::PathBuf>,
    /// The agent dir the trust selector writes to. `None` uses the real one;
    /// tests inject a temp path to stay hermetic.
    pub agent_dir: Option<std::path::PathBuf>,
    /// Whether this project is trusted in the current session (the
    /// "Current session" line of the trust selector).
    pub project_trusted: bool,
}

impl Default for InteractiveOptions {
    fn default() -> Self {
        InteractiveOptions {
            version: env!("CARGO_PKG_VERSION").to_string(),
            theme_name: None,
            tui_mode: None,
            auth_path: None,
            agent_dir: None,
            project_trusted: false,
        }
    }
}

/// App-level key actions the editor intercepts before the text editor sees
/// the key (`CustomEditor.handleInput`, `custom-editor.ts:88-157`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AppAction {
    Clear,
    Exit,
    Interrupt,
    CycleModel(i32),
    ToggleThinking,
    ToolsExpand,
    /// alt+enter (`app.message.followUp`): queue while streaming, else submit.
    FollowUp,
    /// alt+up (`app.message.dequeue`): restore queued messages to the editor.
    Dequeue,
}

/// Events the editor host queues for the orchestrator.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorEventOut {
    Submit(String),
    Action(AppAction),
}

/// The editor host: wraps the editor, intercepts app keybindings, embeds the
/// working status in the top border, and publishes submissions/actions
/// through a shared queue (the tree cannot be downcast generically, so hosts
/// publish outward — the probe pattern from the TUI tests). This type itself
/// opts into downcasting with [`Component::as_any_mut`].
struct ChatEditor {
    editor: Editor,
    events: Arc<Mutex<Vec<EditorEventOut>>>,
}

impl ChatEditor {
    fn new(
        events: Arc<Mutex<Vec<EditorEventOut>>>,
        status: Arc<Mutex<Option<StatusIndicator>>>,
    ) -> Self {
        let mut editor = Editor::new(theme::editor_theme(), EditorOptions::default());
        // pi shows no placeholder in the chat editor.
        editor.set_placeholder(None);
        // The working status rides the top border
        // (`custom-editor.ts:36-79` renderTopBorder override). The slot lives
        // on the orchestrator; the decorator only reads it.
        let decorator_status = status;
        editor.top_border_decorator = Some(Arc::new(move |width, _hidden| {
            let guard = decorator_status.lock().expect("status");
            let Some(indicator) = guard.as_ref() else {
                return String::new();
            };
            let status_text = indicator.render_in_border(width.saturating_sub(5));
            if status_text.is_empty() {
                return String::new();
            }
            let status_width = pi_tui::ansi::visible_width(&status_text);
            if width >= status_width + 5 {
                let theme = theme::theme();
                let border = theme.fg_styler(pi_tui::theme::ThemeColor::BorderMuted);
                return format!(
                    "{}{}{}",
                    border("── "),
                    status_text,
                    border(&format!(" {}", "─".repeat(width - status_width - 4)))
                );
            }
            String::new()
        }));
        ChatEditor { editor, events }
    }
}

impl Component for ChatEditor {
    fn render(&self, width: usize) -> Vec<String> {
        self.editor.render(width)
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        // App keybindings first (`custom-editor.ts:104-153`).
        let action = if theme::app_keys::matches(data, theme::app_keys::CLEAR) {
            Some(AppAction::Clear)
        } else if theme::app_keys::matches(data, theme::app_keys::EXIT) {
            // Exit only when the editor is empty; otherwise the editor
            // deletes forward (`custom-editor.ts:128-136`).
            if self.editor.get_text().is_empty() {
                Some(AppAction::Exit)
            } else {
                None
            }
        } else if theme::app_keys::matches(data, theme::app_keys::INTERRUPT) {
            Some(AppAction::Interrupt)
        } else if theme::app_keys::matches(data, theme::app_keys::MODEL_CYCLE_FORWARD) {
            Some(AppAction::CycleModel(1))
        } else if theme::app_keys::matches(data, theme::app_keys::MODEL_CYCLE_BACKWARD) {
            Some(AppAction::CycleModel(-1))
        } else if theme::app_keys::matches(data, theme::app_keys::THINKING_TOGGLE) {
            Some(AppAction::ToggleThinking)
        } else if theme::app_keys::matches(data, theme::app_keys::TOOLS_EXPAND) {
            Some(AppAction::ToolsExpand)
        } else if theme::app_keys::matches(data, theme::app_keys::MESSAGE_FOLLOW_UP) {
            Some(AppAction::FollowUp)
        } else if theme::app_keys::matches(data, theme::app_keys::MESSAGE_DEQUEUE) {
            Some(AppAction::Dequeue)
        } else {
            None
        };
        if let Some(action) = action {
            self.events
                .lock()
                .expect("editor events")
                .push(EditorEventOut::Action(action));
            return Some(ComponentMutation { consume: true });
        }
        let mutation = Component::handle_input(&mut self.editor, data);
        while let Some(text) = self.editor.take_submitted() {
            self.events
                .lock()
                .expect("editor events")
                .push(EditorEventOut::Submit(text));
        }
        mutation
    }

    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        self.editor.handle_mouse(event)
    }

    fn has_input_handler(&self) -> bool {
        self.editor.has_input_handler()
    }

    fn set_focused(&mut self, focused: bool) {
        self.editor.set_focused(focused);
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// Events flowing from the agent thread to the UI loop.
enum LoopEvent {
    Event(AgentEvent),
    Done,
}

/// Sink that forwards agent events to the UI loop.
struct ChannelSink {
    tx: std::sync::mpsc::Sender<LoopEvent>,
}

impl AgentEventSink for ChannelSink {
    fn emit(&mut self, event: AgentEvent) -> pi_agent::types::BoxFuture<'_, ()> {
        let _ = self.tx.send(LoopEvent::Event(event));
        Box::pin(async {})
    }
}

/// One agent run on a worker thread.
struct AgentRun {
    abort: Arc<AbortSignal>,
    events: std::sync::mpsc::Receiver<LoopEvent>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// The turn's routing wiring under a virtual selection; its pending
    /// `pi.virtual-model-state` entries persist when the run retires.
    #[cfg(feature = "ext-js")]
    routing: Option<Arc<crate::virtual_models::TurnRouting>>,
}

/// Outcome of a compaction worker thread (`compaction_end` event payload).
enum CompactionOutcome {
    Completed(Box<crate::session::compaction::CompactionResult>),
    Failed(String),
    Cancelled,
}

/// One compaction attempt on a worker thread (`session.compact`).
struct CompactionRun {
    abort: Arc<AbortSignal>,
    events: std::sync::mpsc::Receiver<CompactionOutcome>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// `QueueSource` over a mutex-shared [`Queues`]: the loop borrows it for the
/// whole run while the UI keeps enqueueing (`agent.steer`/`followUp`).
struct SharedQueues(Arc<Mutex<Queues>>);

impl pi_agent::types::QueueSource for SharedQueues {
    fn drain_steering(&mut self) -> Vec<AgentMessage> {
        self.0.lock().expect("queues").drain_steering()
    }

    fn drain_follow_up(&mut self) -> Vec<AgentMessage> {
        self.0.lock().expect("queues").drain_follow_up()
    }
}

/// Tool-row bookkeeping: the row's index inside the chat container.
struct PendingTool {
    chat_index: usize,
}

// The flat tree mounts the six hosts in [`HostSlot`] order; the chat
// viewport moves them into its document and dock without changing their
// identities (`switchTuiMode` keeps one component tree,
// `interactive-mode.ts:641-700`).

/// The `steeringMode`/`followUpMode` settings → queue mode. Only "all"
/// switches off the one-at-a-time default (`QueueMode`).
fn queue_mode_from_setting(value: Option<&str>) -> QueueMode {
    if value == Some("all") {
        QueueMode::All
    } else {
        QueueMode::OneAtATime
    }
}

/// The queue mode → settings value.
fn queue_mode_as_setting(mode: QueueMode) -> &'static str {
    match mode {
        QueueMode::All => "all",
        QueueMode::OneAtATime => "one-at-a-time",
    }
}

/// `/name`'s argument: the text after the command word, trimmed
/// (`handleNameCommand`, `interactive-mode.ts:6860`, `/^\/name\s*/` + trim).
fn name_argument(text: &str) -> &str {
    text.strip_prefix("/name").unwrap_or("").trim()
}

/// `JSON.stringify` of a string (`interactive-mode.ts:6875`): a JSON string
/// literal, used for the name-normalization warning.
fn json_stringify(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"))
}

/// The `/session` numbers (`SessionStats`, `agent-session.ts:357-375`,
/// reduced to the fields the command renders; `contextUsage` is footer work).
#[derive(Default)]
struct SessionStats {
    session_file: Option<std::path::PathBuf>,
    session_id: String,
    total_messages: u64,
    user_messages: u64,
    assistant_messages: u64,
    tool_calls: u64,
    tool_results: u64,
    tokens: super::components::footer::UsageTotals,
}

/// `toLocaleString()`'s digit grouping for the token counts
/// (`interactive-mode.ts:6925` and friends). Upstream formats in the host
/// locale; the port always groups with commas (the en-US default).
fn to_locale_string(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.char_indices() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}

/// `renderInfo` (`interactive-mode.ts:6900-6960`): the `/session` block.
#[allow(clippy::too_many_lines)] // mirrors the TS renderInfo; split when extending
fn session_info_text(
    stats: &SessionStats,
    session_name: Option<&str>,
    cache_warming_mode: &str,
    cache_waste: &crate::session::cache_stats::CacheWasteTotals,
    usage_breakdown: &[super::components::footer::UsageCostBreakdownEntry],
    selected_model_key: &str,
) -> String {
    let theme = theme::theme();
    let dim = |text: &str| theme.fg(pi_tui::theme::ThemeColor::Dim, text);
    let tokens = &stats.tokens;
    let mut info = format!("{}\n\n", bold_text("Session Info"));
    if let Some(session_name) = session_name {
        let _ = writeln!(info, "{} {session_name}", dim("Name:"));
    }
    let _ = writeln!(
        info,
        "{} {}",
        dim("File:"),
        stats.session_file.as_deref().map_or_else(
            || "In-memory".to_string(),
            |file| file.to_string_lossy().into_owned()
        )
    );
    let _ = write!(info, "{} {}\n\n", dim("ID:"), stats.session_id);
    let _ = writeln!(info, "{}", bold_text("Messages"));
    let _ = writeln!(info, "{} {}", dim("Total:"), stats.total_messages);
    let _ = writeln!(info, "{} {}", dim("User:"), stats.user_messages);
    let _ = writeln!(info, "{} {}", dim("Assistant:"), stats.assistant_messages);
    let _ = write!(
        info,
        "{} {} calls, {} results\n\n",
        dim("Tools:"),
        stats.tool_calls,
        stats.tool_results
    );
    let _ = writeln!(info, "{}", bold_text("Tokens"));
    // "Input" is the full prompt volume. With cache activity, split it into
    // cached (served from cache) vs uncached (everything else) - the only
    // provider-independent split. Cache writes, where reported, are a detail
    // of the uncached portion.
    let prompt_tokens = tokens.input + tokens.cache_read + tokens.cache_write;
    let _ = writeln!(
        info,
        "{} {}",
        dim("Input:"),
        to_locale_string(prompt_tokens)
    );
    if prompt_tokens > 0 && (tokens.cache_read > 0 || tokens.cache_write > 0) {
        let hit_rate = dim(&format!(
            "({:.1}%)",
            tokens.cache_read as f64 / prompt_tokens as f64 * 100.0
        ));
        let _ = writeln!(
            info,
            "  {} {} {hit_rate}",
            dim("Cached:"),
            to_locale_string(tokens.cache_read)
        );
        let written = if tokens.cache_write > 0 {
            format!(
                " {}",
                dim(&format!(
                    "({} written to cache)",
                    to_locale_string(tokens.cache_write)
                ))
            )
        } else {
            String::new()
        };
        let _ = writeln!(
            info,
            "  {} {}{written}",
            dim("Uncached:"),
            to_locale_string(tokens.input + tokens.cache_write)
        );
    }
    let _ = writeln!(
        info,
        "{} {}",
        dim("Output:"),
        to_locale_string(tokens.output)
    );
    let _ = writeln!(
        info,
        "{} {}",
        dim("Total:"),
        to_locale_string(tokens.tokens())
    );

    let _ = write!(info, "\n{}\n", bold_text("Cache Warming"));
    let _ = writeln!(info, "{} {cache_warming_mode}", dim("Mode:"));
    // The formatted live status lands with the cache warmer; without one the
    // status is always the fallback (`interactive-mode.ts:6934-6935`).
    let _ = writeln!(
        info,
        "{} Inactive (cache warming unavailable)",
        dim("Status:")
    );

    if stats.tokens.cost > 0.0 || cache_waste.missed_tokens > 0 {
        let _ = write!(info, "\n{}\n", bold_text("Cost"));
        let _ = write!(info, "{} ${:.3}", dim("Total:"), stats.tokens.cost);
        // A single entry repeats the total, unless it names a model other
        // than the selected one.
        if usage_breakdown.len() > 1
            || usage_breakdown
                .first()
                .is_some_and(|entry| entry.key != selected_model_key)
        {
            for entry in usage_breakdown {
                let _ = write!(
                    info,
                    "\n  {} ${:.3} {}",
                    dim(&format!("{}:", entry.key)),
                    entry.cost,
                    dim(&format!(
                        "({} tokens)",
                        super::components::footer::format_tokens(entry.tokens)
                    ))
                );
            }
        }
        if cache_waste.missed_tokens > 0 {
            let miss_label = if cache_waste.miss_count == 1 {
                "1 miss".to_string()
            } else {
                format!("{} misses", cache_waste.miss_count)
            };
            let detail = format!(
                "{} tokens, {miss_label}",
                to_locale_string(cache_waste.missed_tokens)
            );
            if cache_waste.missed_cost >= 0.0001 {
                let _ = write!(
                    info,
                    "\n{} ${:.3} {}",
                    dim("Cache Re-billed:"),
                    cache_waste.missed_cost,
                    dim(&format!("({detail})"))
                );
            } else {
                let _ = write!(info, "\n{} {detail}", dim("Cache Re-billed:"));
            }
        }
    }
    info
}

/// `theme.bold` (`theme.ts`): the port's chat lines embed the bold SGR
/// directly (see `compaction_summary.rs`).
fn bold_text(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[22m")
}

/// `handleHotkeysCommand`'s table (`interactive-mode.ts:6999-7099`): the
/// Navigation/Editing/Other sections with the effective keybindings filled
/// in. The extension-registered-shortcuts section (`interactive-mode.ts:7101-7112`)
/// renders only when the runner reports shortcuts; it lands with the
/// extension UI (E3).
#[allow(clippy::too_many_lines)] // mirrors the TS template literal; split when extending
fn hotkeys_markdown() -> String {
    // Navigation keybindings
    let cursor_up = theme::key_display_text("tui.editor.cursorUp");
    let cursor_down = theme::key_display_text("tui.editor.cursorDown");
    let cursor_left = theme::key_display_text("tui.editor.cursorLeft");
    let cursor_right = theme::key_display_text("tui.editor.cursorRight");
    let cursor_word_left = theme::key_display_text("tui.editor.cursorWordLeft");
    let cursor_word_right = theme::key_display_text("tui.editor.cursorWordRight");
    let cursor_line_start = theme::key_display_text("tui.editor.cursorLineStart");
    let cursor_line_end = theme::key_display_text("tui.editor.cursorLineEnd");
    let jump_forward = theme::key_display_text("tui.editor.jumpForward");
    let jump_backward = theme::key_display_text("tui.editor.jumpBackward");
    let page_up = theme::key_display_text("tui.editor.pageUp");
    let page_down = theme::key_display_text("tui.editor.pageDown");

    // Editing keybindings
    let submit = theme::key_display_text("tui.input.submit");
    let new_line = theme::key_display_text("tui.input.newLine");
    let delete_word_backward = theme::key_display_text("tui.editor.deleteWordBackward");
    let delete_word_forward = theme::key_display_text("tui.editor.deleteWordForward");
    let delete_to_line_start = theme::key_display_text("tui.editor.deleteToLineStart");
    let delete_to_line_end = theme::key_display_text("tui.editor.deleteToLineEnd");
    let yank = theme::key_display_text("tui.editor.yank");
    let yank_pop = theme::key_display_text("tui.editor.yankPop");
    let undo = theme::key_display_text("tui.editor.undo");
    let tab = theme::key_display_text("tui.input.tab");

    // App keybindings
    let interrupt = theme::key_display_text(theme::app_keys::INTERRUPT);
    let clear = theme::key_display_text(theme::app_keys::CLEAR);
    let exit = theme::key_display_text(theme::app_keys::EXIT);
    let suspend = theme::key_display_text(theme::app_keys::SUSPEND);
    let cycle_thinking_level = theme::key_display_text(theme::app_keys::THINKING_CYCLE);
    let cycle_model_forward = theme::key_display_text(theme::app_keys::MODEL_CYCLE_FORWARD);
    let select_model = theme::key_display_text(theme::app_keys::MODEL_SELECT);
    let expand_tools = theme::key_display_text(theme::app_keys::TOOLS_EXPAND);
    let toggle_thinking = theme::key_display_text(theme::app_keys::THINKING_TOGGLE);
    let external_editor = theme::key_display_text(theme::app_keys::EDITOR_EXTERNAL);
    let cycle_model_backward = theme::key_display_text(theme::app_keys::MODEL_CYCLE_BACKWARD);
    let copy_message = theme::key_display_text(theme::app_keys::MESSAGE_COPY);
    let follow_up = theme::key_display_text(theme::app_keys::MESSAGE_FOLLOW_UP);
    let dequeue = theme::key_display_text(theme::app_keys::MESSAGE_DEQUEUE);
    let paste_image = theme::key_display_text(theme::app_keys::CLIPBOARD_PASTE_IMAGE);

    let windows_new_line_note = if cfg!(windows) {
        " (Ctrl+Enter on Windows Terminal)"
    } else {
        ""
    };
    format!(
        "
**Navigation**
| Key | Action |
|-----|--------|
| `{cursor_up}` / `{cursor_down}` / `{cursor_left}` / `{cursor_right}` | Move cursor / browse history |
| `{cursor_word_left}` / `{cursor_word_right}` | Move by word |
| `{cursor_line_start}` | Start of line |
| `{cursor_line_end}` | End of line |
| `{jump_forward}` | Jump forward to character |
| `{jump_backward}` | Jump backward to character |
| `{page_up}` / `{page_down}` | Scroll by page |

**Editing**
| Key | Action |
|-----|--------|
| `{submit}` | Send message |
| `{new_line}` | New line{windows_new_line_note} |
| `{delete_word_backward}` | Delete word backwards |
| `{delete_word_forward}` | Delete word forwards |
| `{delete_to_line_start}` | Delete to start of line |
| `{delete_to_line_end}` | Delete to end of line |
| `{yank}` | Paste the most-recently-deleted text |
| `{yank_pop}` | Cycle through the deleted text after pasting |
| `{undo}` | Undo |

**Other**
| Key | Action |
|-----|--------|
| `{tab}` | Path completion / accept autocomplete |
| `{interrupt}` | Cancel autocomplete / abort streaming |
| `{clear}` | Clear editor (first) / exit (second) |
| `{exit}` | Exit (when editor is empty) |
| `{suspend}` | Suspend to background |
| `{cycle_thinking_level}` | Cycle thinking level |
| `{cycle_model_forward}` / `{cycle_model_backward}` | Cycle models |
| `{select_model}` | Open model selector |
| `{expand_tools}` | Toggle tool output expansion |
| `{toggle_thinking}` | Toggle thinking block visibility |
| `{external_editor}` | Edit message in external editor |
| `{copy_message}` | Copy selection or last assistant message |
| `{follow_up}` | Queue follow-up message |
| `{dequeue}` | Restore queued messages |
| `{paste_image}` | Paste files on macOS, images, or text from clipboard |
| `/` | Slash commands |
| `!` | Run bash command |
| `!!` | Run bash command (excluded from context) |
"
    )
}

/// The `modelThinkingLevels` map: `provider/id` → level.
fn model_thinking_levels(settings: &SettingsManager) -> BTreeMap<String, String> {
    settings
        .get("modelThinkingLevels")
        .and_then(|value| value.as_object())
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|level| (key.clone(), level.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The interactive mode (`InteractiveMode`, reduced to the chat core).
// The booleans mirror upstream TS state flags; the port keeps them one-to-one.
#[allow(clippy::struct_excessive_bools)]
pub struct InteractiveMode {
    tui: Tui,
    editor_events: Arc<Mutex<Vec<EditorEventOut>>>,
    status_handle: Arc<Mutex<Option<StatusIndicator>>>,

    session: SessionManager,
    model: pi_ai::types::Model,
    thinking_level: ThinkingLevel,
    api_key: Option<String>,
    access_token: Option<String>,
    stream_fn: Arc<dyn pi_agent::types::StreamFn>,
    /// The composite executor (builtins + extension dispatch, E2 slice 3).
    executor: Arc<dyn ToolExecutor>,
    system_prompt: String,
    /// The extension runner (E2 slice 4): `user_bash` interception,
    /// `session_before_*` gates, the context transform. The TUI surface
    /// itself binds with E3, so dialogs resolve headless here.
    #[cfg(feature = "ext-js")]
    extension_runner: Option<Arc<std::sync::Mutex<pi_ext_js::ExtensionRunner>>>,

    /// Tool rows still awaiting results, keyed by call id.
    pending_tools: std::collections::HashMap<String, PendingTool>,
    /// The streaming assistant row's chat index, when one is open.
    streaming_index: Option<usize>,
    /// The run in flight, if any.
    agent_run: Option<AgentRun>,
    /// The compaction attempt in flight, if any (`session.isCompacting`).
    compaction_run: Option<CompactionRun>,
    /// Messages queued while compaction runs
    /// (`compactionQueuedMessages`), `(text, follow_up)`.
    compaction_queued: Vec<(String, bool)>,
    /// ctrl+c twice within 500ms exits (`handleCtrlC`).
    last_sigint_time: Option<Instant>,
    /// Spinner-frame pacing, carried across `pump` calls.
    last_spinner_tick: Instant,
    /// ctrl+o state: tool output expansion and the header hints.
    tool_output_expanded: bool,
    hide_thinking_block: bool,
    exiting: bool,
    /// The mounted `/model` selector's event queue, while open.
    model_selector_events: Option<Arc<Mutex<Vec<ModelSelectorEvent>>>>,
    /// Events from the mounted `/trust` selector, same queue pattern.
    trust_selector_events: Option<Arc<Mutex<Vec<TrustSelectorEvent>>>>,
    /// Events from the mounted `/settings` dialog, same queue pattern.
    settings_selector_events: Option<Arc<Mutex<Vec<SettingsSelectorEvent>>>>,
    session_selector_events: Option<Arc<Mutex<Vec<SessionSelectorEvent>>>>,
    /// Events from the mounted `/login`//`/logout` selector
    /// (`OAuthSelectorComponent`'s onSelect/onCancel).
    auth_selector_events: Option<Arc<Mutex<Vec<AuthSelectorEvent>>>>,
    /// Which flow the mounted auth selector serves ("login"/"logout") —
    /// upstream binds the behavior per instance; the port's single event
    /// queue needs the tag.
    auth_selector_mode: Option<&'static str>,
    /// The auth-type filter the open provider selector was seeded with
    /// (`showLoginProviderSelector`'s `authType`).
    pending_auth_type: Option<&'static str>,
    /// The one-provider entries behind an open auth-type menu
    /// (`showLoginAuthTypeSelector`'s `providerOptions`).
    pending_auth_type_options: Option<Vec<AuthSelectorProvider>>,
    /// Events from the mounted login prompt (`LoginDialogComponent`'s
    /// input resolver/rejecter), while an API-key login runs.
    login_prompt_events: Option<Arc<Mutex<Vec<LoginPromptEvent>>>>,
    /// The provider the open login prompt configures
    /// (`loginProvider`'s closure state).
    login_prompt_provider: Option<(String, String)>,
    /// Where a cancelled login returns to (`onBack`).
    login_back: Option<LoginBackStep>,
    /// The editor, parked while the selector occupies its tree slot.
    suspended_editor: Option<Box<dyn Component>>,
    /// The resolved renderer mode (`options.tuiMode`): which screen the
    /// frames paint onto (`createInteractiveTui`, `tui-renderer.js:8`).
    tui_mode: TuiMode,
    /// The editor host's component id. Focus follows it across the mode
    /// switch, exactly as `switchTuiMode` carries the focused component
    /// between renderers (`interactive-mode.ts:646, 677`).
    editor_id: ComponentId,
    /// The mounted chat viewport while fullscreen
    /// (`this.fullscreenLayoutRoot` / `this.transcriptScrollView`).
    viewport: Option<ChatViewport>,
    /// The playing 3D easter egg overlay, if any (`playEasterEgg3d`). The
    /// host owns it because the port has no timers: `pump` refreshes the
    /// terminal rows and hides the overlay once the exit completes.
    easter_egg: Option<EasterEggHandle>,
    /// Header-logo clicks queued by the header's callback, which cannot
    /// borrow the mode during dispatch (`header.onLogoClick`,
    /// `interactive-mode.ts:792`).
    easter_egg_requests: Arc<Mutex<Vec<(i64, i64)>>>,
    /// Where provider credentials live (`auth.json`).
    auth_path: std::path::PathBuf,
    /// The agent dir the trust selector reads and writes (`trust.json`).
    agent_dir: std::path::PathBuf,
    /// Whether this project is trusted in the current session.
    project_trusted: bool,
    /// The settings manager the dialog reads and writes
    /// (`this.settingsManager`, `interactive-mode.ts`).
    settings: SettingsManager,
    /// The agent loop's steering/follow-up queues, shared with the worker
    /// thread and persistent across runs (upstream: `agent.queues`).
    queues: Arc<Mutex<Queues>>,
    /// Queued messages awaiting loop delivery, with their queue mode; they
    /// persist to the transcript when the loop declares them.
    queued_deliveries: Vec<(bool, AgentMessage)>,
    /// The running prompt's `(timestamp, text)`: its `MessageEnd` is skipped
    /// (it persisted at submit) so same-millisecond steering cannot steal it.
    active_prompt: Option<(i64, String)>,
    /// pi's stdin buffer: raw stdin bytes decode into complete sequences
    /// (`stdin_buffer.rs`), terminal replies included — crossterm's event
    /// parser would eat them and never deliver them.
    stdin_buffer: StdinBuffer,
    /// Undecoded UTF-8 carried over between chunks.
    stdin_pending: Vec<u8>,
    /// When the last stdin chunk arrived, for the buffer's sequence timeout.
    stdin_last_input: Option<Instant>,
    /// The theme setting in force: `--use-theme`, else settings.json's
    /// `theme` (a name, or a `light/dark` pair). Upstream keeps the settings
    /// manager itself in the controller.
    theme_setting: Option<String>,
    /// The active theme's name (`theme-controller.ts`).
    theme_active_name: String,
    /// Whether mode-2031 reports re-apply the theme (system/auto themes).
    theme_auto_sync: bool,
    /// Late OSC color replies, pushed by the query's `onLateReply` sink.
    theme_late_reports: Arc<Mutex<Vec<TerminalColors>>>,
    /// Mode-2031 reports from the TUI's scheme listener.
    theme_scheme_reports: Arc<Mutex<Vec<TerminalColorScheme>>>,
    /// When the current color query was issued — the 100 ms deadline clock.
    theme_query_start: Option<Instant>,
}

impl InteractiveMode {
    #[allow(clippy::too_many_lines)] // the TS constructor's mount + settings flow; split when extending
    fn new(
        runtime: &mut PrintRuntime,
        options: &InteractiveOptions,
        terminal: Box<dyn pi_tui::terminal::Terminal>,
    ) -> Self {
        // The controller's constructor (`theme-controller.ts:76-91`): mark
        // colors pending — the system theme's first frame renders grayscale —
        // then resolve the setting and load that theme.
        theme::mark_terminal_colors_pending();
        let theme_setting = options.theme_name.clone();
        let theme_active_name = theme::resolve_theme_setting(theme_setting.as_deref())
            .unwrap_or_else(|| pi_tui::theme::SYSTEM_THEME_NAME.to_string());
        theme::init_theme(Some(&theme_active_name));
        let theme_auto_sync = pi_tui::theme::parse_auto_theme_setting(theme_setting.as_deref())
            .is_some()
            || theme_active_name == pi_tui::theme::SYSTEM_THEME_NAME;
        let theme_late_reports: Arc<Mutex<Vec<TerminalColors>>> = Arc::new(Mutex::new(Vec::new()));
        let theme_scheme_reports: Arc<Mutex<Vec<TerminalColorScheme>>> =
            Arc::new(Mutex::new(Vec::new()));

        let editor_events = Arc::new(Mutex::new(Vec::new()));
        let status_handle: Arc<Mutex<Option<StatusIndicator>>> = Arc::new(Mutex::new(None));
        let mut editor_host =
            ChatEditor::new(Arc::clone(&editor_events), Arc::clone(&status_handle));

        // `setupAutocompleteProvider` (`interactive-mode.ts:855-871`): the
        // base provider is built once from the builtin commands, the session
        // cwd, and the fd binary.
        #[cfg(feature = "ext-js")]
        let virtual_catalog_models = runtime
            .extension_runner
            .as_ref()
            .map(|runner| {
                crate::virtual_models::virtual_models_from_defs(
                    &runner
                        .lock()
                        .expect("extension runner mutex poisoned")
                        .virtual_models(),
                )
            })
            .unwrap_or_default();
        #[cfg(not(feature = "ext-js"))]
        let virtual_catalog_models: Vec<pi_ai::types::Model> = Vec::new();
        let provider = autocomplete::CombinedAutocompleteProvider::new(
            Self::autocomplete_commands(&virtual_catalog_models),
            runtime.session.get_cwd().to_path_buf(),
            crate::tools::bash::which("fd"),
        );
        editor_host
            .editor
            .set_autocomplete_provider(Some(Arc::new(provider)));

        // The header's logo-click queue (`header.onLogoClick`): the callback
        // cannot borrow the mode, so it parks the logo's position here.
        let easter_egg_requests: Arc<Mutex<Vec<(i64, i64)>>> = Arc::new(Mutex::new(Vec::new()));
        let (mut tui, editor_id) = Self::mount_flat_tree(
            terminal,
            &options.version,
            &runtime.model.id,
            editor_host,
            FooterComponent::new(Self::footer_data(runtime)),
            &easter_egg_requests,
        );

        let theme_query_start = Self::arm_theme_queries(
            &mut tui,
            theme_auto_sync,
            &theme_late_reports,
            &theme_scheme_reports,
        );
        let (agent_dir, settings, hide_thinking_block, queues) =
            Self::startup_settings(options, runtime);
        // The renderer choice: the `--tui-mode` flag wins, otherwise the
        // settings (`getTuiMode`, default fullscreen).
        let tui_mode = options.tui_mode.unwrap_or_else(|| settings.get_tui_mode());

        InteractiveMode {
            tui,
            editor_events,
            status_handle,
            session: std::mem::replace(
                &mut runtime.session,
                SessionManager::in_memory(
                    std::path::Path::new("/"),
                    &NewSessionOptions::default(),
                    Vec::new(),
                )
                .expect("in-memory session"),
            ),
            model: runtime.model.clone(),
            thinking_level: runtime.thinking_level,
            api_key: runtime.api_key.clone(),
            access_token: runtime.access_token.clone(),
            stream_fn: Arc::clone(&runtime.stream_fn),
            executor: Arc::clone(&runtime.executor),
            system_prompt: runtime.system_prompt.clone(),
            #[cfg(feature = "ext-js")]
            extension_runner: runtime.extension_runner.clone(),
            pending_tools: std::collections::HashMap::new(),
            streaming_index: None,
            agent_run: None,
            last_sigint_time: None,
            last_spinner_tick: Instant::now(),
            tool_output_expanded: false,
            hide_thinking_block,
            exiting: false,
            model_selector_events: None,
            trust_selector_events: None,
            settings_selector_events: None,
            session_selector_events: None,
            auth_selector_events: None,
            auth_selector_mode: None,
            pending_auth_type: None,
            pending_auth_type_options: None,
            login_prompt_events: None,
            login_prompt_provider: None,
            login_back: None,
            suspended_editor: None,
            tui_mode,
            editor_id,
            viewport: None,
            easter_egg: None,
            easter_egg_requests,
            auth_path: options
                .auth_path
                .clone()
                .unwrap_or_else(crate::config::get_auth_path),
            agent_dir,
            project_trusted: options.project_trusted,
            settings,
            queues,
            queued_deliveries: Vec::new(),
            compaction_run: None,
            compaction_queued: Vec::new(),
            active_prompt: None,
            stdin_buffer: StdinBuffer::new(StdinBufferOptions::default()),
            stdin_pending: Vec::new(),
            stdin_last_input: None,
            theme_setting,
            theme_active_name,
            theme_auto_sync,
            theme_late_reports,
            theme_scheme_reports,
            theme_query_start,
        }
    }

    /// The regular renderer's flat mount: the six hosts in [`HostSlot`] order
    /// (`interactive-mode.ts` init's `mountInteractiveTui` flat path), focus
    /// on the editor.
    fn mount_flat_tree(
        terminal: Box<dyn pi_tui::terminal::Terminal>,
        version: &str,
        model_id: &str,
        editor_host: ChatEditor,
        footer: FooterComponent,
        easter_egg_requests: &Arc<Mutex<Vec<(i64, i64)>>>,
    ) -> (Tui, ComponentId) {
        let mut tui = Tui::new(terminal);
        tui.set_clear_on_shrink(true);
        let mut header = Self::build_header(version, model_id);
        // `header.onLogoClick = …` (`interactive-mode.ts:792`): the 3D pi
        // logo lifts off the logo's screen position. The callback cannot
        // borrow the mode, so it queues the position for `pump`.
        let requests = Arc::clone(easter_egg_requests);
        header.set_on_logo_click(Some(Box::new(move |column, row| {
            if let Ok(mut pending) = requests.lock() {
                pending.push((column, row));
            }
        })));
        tui.add_child(Box::new(header));
        tui.add_child(Box::new(pi_tui::component::Container::new()));
        tui.add_child(Box::new(pi_tui::component::Container::new()));
        tui.add_child(Box::new(IdleStatus));
        // Ids are heap addresses, so the editor's identity is taken once the
        // host is boxed — focus names it across the mode switches.
        let editor_box = Box::new(editor_host);
        let editor_id = component_id(editor_box.as_ref());
        tui.add_child(editor_box);
        tui.add_child(Box::new(footer));
        tui.set_focus(Some(FocusTarget::Id(editor_id)));
        (tui, editor_id)
    }

    /// Arms the theme controller's terminal plumbing: the mode-2031 scheme
    /// listener, then the initial color query with its late-reply sink. The
    /// closures cannot borrow the mode, so they queue into the mode's
    /// report lists. Returns the query's start instant — the deadline clock.
    fn arm_theme_queries(
        tui: &mut Tui,
        auto_sync: bool,
        late_reports: &Arc<Mutex<Vec<TerminalColors>>>,
        scheme_reports: &Arc<Mutex<Vec<TerminalColorScheme>>>,
    ) -> Option<Instant> {
        // Mode-2031 reports reach the controller through a listener the TUI
        // dispatches (`bindTerminalColorSchemeListener`).
        let scheme_queue = Arc::clone(scheme_reports);
        tui.add_color_scheme_listener(Arc::new(move |scheme| {
            if let Ok(mut pending) = scheme_queue.lock() {
                pending.push(scheme);
            }
        }));
        // The initial color query (`applyFromSettings` →
        // `requestTerminalColors`); late replies ride the sink.
        let query_start = Some(Instant::now());
        let late_queue = Arc::clone(late_reports);
        let sink = Box::new(move |colors: TerminalColors| {
            if let Ok(mut pending) = late_queue.lock() {
                pending.push(colors);
            }
        });
        let _ = tui.query_terminal_colors(100, Some(sink));
        tui.set_color_scheme_notifications(auto_sync);
        query_start
    }

    /// Seeds the session flags from settings (`interactive-mode.ts`'s
    /// constructor reads them through the settings manager): hide-thinking,
    /// and the steering/follow-up queue modes.
    fn startup_settings(
        options: &InteractiveOptions,
        runtime: &PrintRuntime,
    ) -> (
        std::path::PathBuf,
        SettingsManager,
        bool,
        Arc<Mutex<Queues>>,
    ) {
        let agent_dir = options
            .agent_dir
            .clone()
            .unwrap_or_else(crate::config::get_agent_dir);
        let session_cwd = runtime.session.get_cwd().to_path_buf();
        let settings = SettingsManager::create(
            &session_cwd,
            &agent_dir,
            SettingsManagerCreateOptions {
                project_trusted: Some(options.project_trusted),
            },
        );
        let hide_thinking_block = settings
            .get("hideThinkingBlock")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let steering = settings
            .get("steeringMode")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let follow_up = settings
            .get("followUpMode")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let queues = Arc::new(Mutex::new(Queues::new()));
        {
            let mut pending = queues.lock().expect("queues");
            pending.set_steering_mode(queue_mode_from_setting(steering.as_deref()));
            pending.set_follow_up_mode(queue_mode_from_setting(follow_up.as_deref()));
        }
        (agent_dir, settings, hide_thinking_block, queues)
    }

    /// The `/`-commands offered by autocomplete (`createBaseAutocompleteProvider`,
    /// `interactive-mode.ts:743-853`). Only the commands this port implements
    /// are listed — advertising upstream's unimplemented builtins would make
    /// the picker lie. Argument completions: `/model` fuzzy-searches the
    /// whole catalog (upstream: the scoped-models subset or the runtime
    /// snapshot; the scoped-models setting lands later), `/thinking` the
    /// valid levels.
    fn autocomplete_commands(
        virtual_models: &[pi_ai::types::Model],
    ) -> Vec<autocomplete::SlashCommand> {
        let virtual_models = virtual_models.to_vec();
        let commands = vec![
            autocomplete::SlashCommand::new("model", "Select model (opens selector UI)")
                .with_argument_hint("<provider/model>")
                .with_argument_completions(Arc::new(move |prefix: &str| {
                    let composed = crate::models_state::all();
                    let mut models: Vec<(String, String, String)> = Vec::new(); // (id, provider, name)
                    for provider in crate::models_state::providers() {
                        for model in composed.iter().filter(|model| model.provider == provider) {
                            // A virtual registration hides the same-id
                            // physical entry (`withVirtualModels`).
                            if virtual_models.iter().any(|virtual_model| {
                                virtual_model.provider == provider && virtual_model.id == model.id
                            }) {
                                continue;
                            }
                            models.push((model.id.clone(), provider.clone(), model.name.clone()));
                        }
                    }
                    for model in &virtual_models {
                        models.push((model.id.clone(), model.provider.clone(), model.name.clone()));
                    }
                    // `getModelSearchText` (`model-search.ts:7-11`).
                    let filtered =
                        pi_tui::fuzzy::fuzzy_filter(&models, prefix, |(id, provider, name)| {
                            if name.is_empty() {
                                format!("{id} {provider} {provider}/{id} {provider} {id}")
                            } else {
                                format!("{id} {provider} {provider}/{id} {provider} {id} {name}")
                            }
                        });
                    let items: Vec<pi_tui::components::select_list::SelectItem> = filtered
                        .into_iter()
                        .map(
                            |(id, provider, _name)| pi_tui::components::select_list::SelectItem {
                                value: format!("{provider}/{id}"),
                                label: id,
                                description: Some(provider),
                            },
                        )
                        .collect();
                    if items.is_empty() { None } else { Some(items) }
                })),
            autocomplete::SlashCommand::new("compact", "Manually compact the session context"),
            autocomplete::SlashCommand::new("trust", "Show the project trust options"),
            autocomplete::SlashCommand::new("settings", "Open settings menu"),
            autocomplete::SlashCommand::new("resume", "Resume another session"),
            // `BUILTIN_SLASH_COMMANDS` (`slash-commands.ts:20-43`) descriptions
            // and hints, verbatim. `/bug` stays unadvertised: its flow needs
            // the bug-report bundle, upload, and archive infrastructure.
            autocomplete::SlashCommand::new("copy", "Copy last agent message to clipboard"),
            autocomplete::SlashCommand::new("name", "Set session display name"),
            autocomplete::SlashCommand::new("session", "Show session info and stats"),
            autocomplete::SlashCommand::new("changelog", "Show changelog entries"),
            autocomplete::SlashCommand::new("hotkeys", "Show all keyboard shortcuts"),
            // `BUILTIN_SLASH_COMMANDS` (`slash-commands.ts:37-38`); the
            // `/login` argument completions ride
            // `loginCommand.getArgumentCompletions`
            // (`interactive-mode.ts:801-812`).
            autocomplete::SlashCommand::new("login", "Configure provider authentication")
                .with_argument_hint("<provider>")
                .with_argument_completions(Arc::new(|prefix: &str| {
                    let options = oauth::login_provider_options(&crate::config::get_auth_path());
                    let completions = oauth::login_provider_completion_options(&options);
                    let filtered = pi_tui::fuzzy::fuzzy_filter(
                        &completions,
                        prefix,
                        oauth::login_provider_search_text,
                    );
                    let items: Vec<pi_tui::components::select_list::SelectItem> = filtered
                        .into_iter()
                        .map(|option| {
                            let description = oauth::login_provider_completion_description(&option);
                            pi_tui::components::select_list::SelectItem {
                                value: option.id.clone(),
                                label: option.id,
                                description: Some(description),
                            }
                        })
                        .collect();
                    if items.is_empty() { None } else { Some(items) }
                })),
            autocomplete::SlashCommand::new("logout", "Remove provider authentication"),
            autocomplete::SlashCommand::new("thinking", "Set thinking level")
                .with_argument_hint("<level>")
                .with_argument_completions(Arc::new(|prefix: &str| {
                    let items: Vec<pi_tui::components::select_list::SelectItem> =
                        crate::cli::args::VALID_THINKING_LEVELS
                            .iter()
                            .filter(|level| level.starts_with(prefix))
                            .map(|level| {
                                pi_tui::components::select_list::SelectItem::new(*level, *level)
                            })
                            .collect();
                    if items.is_empty() { None } else { Some(items) }
                })),
            autocomplete::SlashCommand::new("new", "Start a new session"),
            autocomplete::SlashCommand::new("quit", "Quit pi"),
        ];
        commands
    }

    /// The startup header (`interactive-mode.ts:1049-1122`): the logo carries
    /// the version, its second line the hints; ctrl+o expands.
    fn build_header(version: &str, _model_id: &str) -> BuiltInHeader {
        let (logo_top, logo_bottom) = pi_logo_lines();
        let with_logo = |hints: String| {
            format!(
                "{logo_top} {}\n{logo_bottom} {hints}",
                theme::theme().fg(pi_tui::theme::ThemeColor::Dim, &format!("v{version}"))
            )
        };
        let expanded_instructions = [
            theme::key_hint(theme::app_keys::INTERRUPT, "to interrupt"),
            theme::key_hint(theme::app_keys::CLEAR, "to clear"),
            theme::raw_key_hint(
                &format!("{} twice", theme::key_text(theme::app_keys::CLEAR)),
                "to exit",
            ),
            theme::key_hint(theme::app_keys::EXIT, "to exit (empty)"),
            theme::key_hint(theme::app_keys::SUSPEND, "to suspend"),
            theme::key_hint(theme::app_keys::THINKING_CYCLE, "to cycle thinking level"),
            theme::raw_key_hint(
                &format!(
                    "{}/{}",
                    theme::key_text(theme::app_keys::MODEL_CYCLE_FORWARD),
                    theme::key_text(theme::app_keys::MODEL_CYCLE_BACKWARD)
                ),
                "to cycle models",
            ),
            theme::raw_key_hint("/", "for commands"),
            theme::raw_key_hint("!", "to run bash"),
            theme::raw_key_hint("!!", "to run bash (no context)"),
            theme::key_hint(theme::app_keys::TOOLS_EXPAND, "to expand tools"),
            theme::key_hint(theme::app_keys::THINKING_TOGGLE, "to expand thinking"),
        ]
        .join("\n");
        let compact_instructions = [
            theme::key_hint(theme::app_keys::INTERRUPT, "interrupt"),
            theme::raw_key_hint(
                &format!(
                    "{}/{}",
                    theme::key_text(theme::app_keys::CLEAR),
                    theme::key_text(theme::app_keys::EXIT)
                ),
                "clear/exit",
            ),
            theme::raw_key_hint("/", "commands"),
            theme::raw_key_hint("!", "bash"),
            theme::key_hint(theme::app_keys::TOOLS_EXPAND, "more"),
        ]
        .join(&theme::theme().fg(pi_tui::theme::ThemeColor::Muted, " · "));
        let onboarding = theme::theme().fg(
            pi_tui::theme::ThemeColor::Dim,
            "Pi can explain its own features and look up its docs. Ask it how to use or extend Pi.",
        );
        BuiltInHeader::new(
            format!(
                "{}\n{}\n\n{}",
                with_logo(compact_instructions),
                theme::theme().fg(
                    pi_tui::theme::ThemeColor::Dim,
                    "Press ctrl+o to show full startup help."
                ),
                onboarding
            ),
            format!("{}\n\n{}", with_logo(expanded_instructions), onboarding),
            false,
            0,
            0,
        )
    }

    fn footer_data(runtime: &PrintRuntime) -> FooterData {
        FooterData {
            cwd: runtime.session.get_cwd().to_string_lossy().to_string(),
            git_branch: git_branch(runtime.session.get_cwd()),
            session_name: None,
            totals: UsageTotals::default(),
            latest_cache_hit_rate: None,
            context_percent: None,
            context_window: runtime.model.context_window,
            model_id: runtime.model.id.clone(),
            model_provider: runtime.model.provider.clone(),
            model_supports_reasoning: runtime.model.reasoning,
            thinking_level: runtime.thinking_level.as_str().to_string(),
            auto_compact_enabled: false,
        }
    }

    // --------------------------------------------------------------
    // Tree access
    // --------------------------------------------------------------

    fn chat_mut(&mut self) -> &mut pi_tui::component::Container {
        self.downcast_tree::<pi_tui::component::Container>(HostSlot::Chat)
            .expect("chat container")
    }

    /// A slot's host, wherever the viewport has it: a flat root child in the
    /// regular renderer, nested inside the chat viewport's document or dock
    /// in fullscreen. `switchTuiMode` keeps one component tree, so the
    /// helpers reach the hosts by recorded identity in either arrangement.
    fn downcast_tree<T: 'static>(&mut self, slot: HostSlot) -> Option<&mut T> {
        let id = self
            .viewport
            .as_ref()
            .map(|viewport| viewport.slot_id(slot));
        if let Some(id) = id {
            return self
                .tui
                .component_by_id_deep_mut(id)?
                .as_any_mut()?
                .downcast_mut::<T>();
        }
        self.tui
            .child_mut(slot.index())?
            .as_any_mut()?
            .downcast_mut::<T>()
    }

    fn downcast_chat<T: 'static>(&mut self, index: usize) -> Option<&mut T> {
        self.downcast_tree::<pi_tui::component::Container>(HostSlot::Chat)
            .and_then(|chat| chat.get_mut(index))
            .and_then(|child| child.as_any_mut())
            .and_then(|any| any.downcast_mut::<T>())
    }

    fn add_to_chat(&mut self, component: Box<dyn Component>) -> usize {
        let chat = self.chat_mut();
        chat.add_child(component);
        chat.len() - 1
    }

    fn chat_len(&mut self) -> usize {
        self.chat_mut().len()
    }

    fn editor_mut(&mut self) -> &mut ChatEditor {
        self.downcast_tree::<ChatEditor>(HostSlot::Editor)
            .expect("editor host")
    }

    // --------------------------------------------------------------
    // Message rendering (`addMessageToChat`, `interactive-mode.ts:4008`)
    // --------------------------------------------------------------

    fn add_user_message(&mut self, text: &str) {
        if self.chat_len() > 0 {
            self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        }
        self.add_to_chat(Box::new(UserMessageComponent::new(text, 1)));
        self.tui.request_render();
    }

    /// `renderInitialMessages` (`interactive-mode.ts:3589-3592`): replay the
    /// resumed session's compaction-aware context entries into the chat so a
    /// `--continue`/`--session` start shows the prior transcript.
    fn render_initial_messages(&mut self) {
        let entries = self.session.build_context_entries();
        self.render_session_entries(&entries, true);
        self.refresh_footer();
        self.tui.request_render();
    }

    /// `renderSessionEntries` + `renderSessionItems` (`interactive-mode.ts:3437-3464`,
    /// `:3340-3425`): walk session entries and re-create the chat rows. The
    /// compaction summary and its billed-tokens notice come from the
    /// `compaction` entry; assistant tool calls open tool rows that later
    /// `toolResult` messages complete; rows with no result stay pending, like
    /// upstream's `pendingTools` handoff.
    ///
    /// Deviations: entries with no ported component are skipped —
    /// `custom_message`, `branch_summary` (forking is unported), and the
    /// cache-miss/cache-warm usage notices (`showCacheMissNotices` is not
    /// ported). `bashExecution` rows likewise (upstream's `!` persistence).
    fn render_session_entries(
        &mut self,
        entries: &[crate::session::records::SessionEntry],
        populate_history: bool,
    ) {
        self.pending_tools.clear();
        let mut rendered_pending: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for entry in entries {
            match entry {
                crate::session::records::SessionEntry::Message(message_entry) => {
                    self.render_replayed_message(
                        &message_entry.message,
                        &mut rendered_pending,
                        populate_history,
                    );
                }
                crate::session::records::SessionEntry::Compaction(compaction) => {
                    self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
                    let mut row =
                        super::components::compaction_summary::CompactionSummaryMessageComponent::new(
                            &compaction.summary,
                            compaction.tokens_before as u64,
                            1,
                        );
                    row.set_expanded(self.tool_output_expanded);
                    self.add_to_chat(Box::new(row));
                    if let Some(usage) = &compaction.usage {
                        let tokens =
                            usage.input + usage.output + usage.cache_read + usage.cache_write;
                        let cost = if usage.cost.total >= 0.01 {
                            format!(" (~${:.2})", usage.cost.total)
                        } else {
                            String::new()
                        };
                        self.add_to_chat(Box::new(Text::new(
                            theme::theme().fg(
                                pi_tui::theme::ThemeColor::Warning,
                                &format!(
                                    "Compaction: {} tokens billed{cost}",
                                    super::components::footer::format_tokens(tokens)
                                ),
                            ),
                            1,
                            0,
                        )));
                    }
                }
                // `sessionEntryToContextMessages` returns [] for every other
                // entry type this port models.
                _ => {}
            }
        }
        // Unmatched tool rows stay open — upstream moves them into
        // `pendingTools` so late results still land.
        for (tool_call_id, index) in rendered_pending {
            self.pending_tools
                .insert(tool_call_id, PendingTool { chat_index: index });
        }
    }

    /// The per-message branch of the replay (`addMessageToChat` +
    /// `renderSessionItems`, `interactive-mode.ts:3261-3338`, `:3359-3416`).
    fn render_replayed_message(
        &mut self,
        message: &AgentMessage,
        rendered_pending: &mut std::collections::HashMap<String, usize>,
        populate_history: bool,
    ) {
        match message {
            AgentMessage::User(user) => {
                let text = match &user.content {
                    pi_ai::types::UserContent::Text(text) => text.clone(),
                    pi_ai::types::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            pi_ai::types::InputContent::Text(text) => Some(text.text.clone()),
                            pi_ai::types::InputContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                if !text.trim().is_empty() {
                    if self.chat_len() > 0 {
                        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
                    }
                    self.add_to_chat(Box::new(UserMessageComponent::new(&text, 1)));
                    if populate_history {
                        self.editor_mut().editor.add_to_history(&text);
                    }
                }
            }
            AgentMessage::Assistant(assistant) => {
                let mut component =
                    AssistantMessageComponent::new(self.hide_thinking_block, "Thinking...", 1);
                component.update_content(assistant, false);
                self.add_to_chat(Box::new(component));
                let aborted = assistant.stop_reason == pi_ai::types::StopReason::Aborted;
                let failed = aborted || assistant.stop_reason == pi_ai::types::StopReason::Error;
                for block in &assistant.content {
                    let pi_ai::types::AssistantContent::ToolCall(call) = block else {
                        continue;
                    };
                    let mut tool = ToolExecutionComponent::new(
                        &call.name,
                        &call.id,
                        serde_json::Value::Object(call.arguments.clone()),
                        1,
                        &self.session.get_cwd().to_string_lossy(),
                        crate::tools::renderers::renderers_for(&call.name),
                    );
                    tool.set_args_complete();
                    tool.set_expanded(self.tool_output_expanded);
                    if failed {
                        // `renderSessionItems` (`:3389-3400`): aborted rows
                        // show the abort note, errored rows the error text.
                        let error = if aborted {
                            "Operation aborted".to_string()
                        } else {
                            assistant
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "Error".to_string())
                        };
                        tool.update_result(
                            ToolResultView {
                                text: error,
                                is_error: true,
                                duration_ms: None,
                            },
                            false,
                        );
                        self.add_to_chat(Box::new(tool));
                    } else {
                        let index = self.add_to_chat(Box::new(tool));
                        rendered_pending.insert(call.id.clone(), index);
                    }
                }
            }
            AgentMessage::ToolResult(result) => {
                let Some(index) = rendered_pending.remove(&result.tool_call_id) else {
                    return;
                };
                let blocks: Vec<pi_ai::types::ContentBlock> = result
                    .content
                    .iter()
                    .cloned()
                    .map(pi_ai::types::ContentBlock::from)
                    .collect();
                let text = crate::tools::render_utils::get_text_output(&blocks, true);
                if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(index) {
                    component.update_result(
                        ToolResultView {
                            text,
                            is_error: result.is_error,
                            duration_ms: result.duration_ms,
                        },
                        false,
                    );
                }
            }
            // System messages render nothing upstream either
            // (`case "system": break`); custom/bash/branch/compaction-summary
            // messages have no ported component.
            _ => {}
        }
    }

    fn start_assistant_stream(&mut self) {
        let component = AssistantMessageComponent::new(self.hide_thinking_block, "Thinking...", 1);
        self.streaming_index = Some(self.add_to_chat(Box::new(component)));
        self.tui.request_render();
    }

    fn update_assistant_stream(&mut self, message: &pi_ai::types::AssistantMessage) {
        if let Some(index) = self.streaming_index
            && let Some(component) = self.downcast_chat::<AssistantMessageComponent>(index)
        {
            component.update_content(message, true);
        }
        // Open a tool row per new call (`interactive-mode.ts:3654-3680`).
        for block in &message.content {
            let pi_ai::types::AssistantContent::ToolCall(call) = block else {
                continue;
            };
            if self.pending_tools.contains_key(&call.id) {
                continue;
            }
            let component = ToolExecutionComponent::new(
                &call.name,
                &call.id,
                serde_json::Value::Object(call.arguments.clone()),
                1,
                &self.session.get_cwd().to_string_lossy(),
                crate::tools::renderers::renderers_for(&call.name),
            );
            let index = self.add_to_chat(Box::new(component));
            self.pending_tools
                .insert(call.id.clone(), PendingTool { chat_index: index });
        }
        self.tui.request_render();
    }

    // --------------------------------------------------------------
    // Agent events (`handleEvent`, `interactive-mode.ts:3531`)
    // --------------------------------------------------------------

    #[allow(clippy::too_many_lines)]
    fn handle_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentStart
            | AgentEvent::TurnStart
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::ToolExecutionEnd { .. }
            | AgentEvent::AutoRetryStart { .. }
            | AgentEvent::AutoRetryEnd { .. }
            | AgentEvent::CompactionStart { .. }
            | AgentEvent::CompactionEnd { .. } => {
                // The final tool result renders with its message end. The
                // pump drives its runs directly; the retry and compaction
                // status displays land with the pump's AgentRun rework
                // (upstream shows them via agent-session events).
            }
            AgentEvent::MessageStart { message } => match message {
                AgentMessage::User(user) => {
                    let text = match &user.content {
                        pi_ai::types::UserContent::Text(text) => text.clone(),
                        pi_ai::types::UserContent::Blocks(blocks) => blocks
                            .iter()
                            .filter_map(|block| match block {
                                pi_ai::types::InputContent::Text(text) => Some(text.text.clone()),
                                pi_ai::types::InputContent::Image(_) => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    if !text.trim().is_empty() {
                        self.add_user_message(&text);
                    }
                }
                AgentMessage::Assistant(_) => self.start_assistant_stream(),
                _ => {}
            },
            AgentEvent::MessageUpdate { message, .. } => {
                self.update_assistant_stream(message);
            }
            AgentEvent::MessageEnd { message } => match message {
                AgentMessage::User(user) => {
                    // Queued (steer/follow-up) deliveries persist when the
                    // loop declares them, so the transcript order matches
                    // upstream (`agent-session.ts` appends at declaration);
                    // the run's own first prompt persisted at submit. The
                    // active-prompt marker tells them apart even when a
                    // steering message shares its millisecond.
                    let text = match &user.content {
                        pi_ai::types::UserContent::Text(text) => Some(text.clone()),
                        pi_ai::types::UserContent::Blocks(_) => None,
                    };
                    let is_active_prompt = text.is_some()
                        && self
                            .active_prompt
                            .as_ref()
                            .is_some_and(|(timestamp, prompt)| {
                                *timestamp == user.timestamp && Some(prompt) == text.as_ref()
                            });
                    if is_active_prompt {
                        self.active_prompt = None;
                    } else {
                        let delivered =
                            self.queued_deliveries
                                .iter()
                                .position(|(_, queued)| match queued {
                                    AgentMessage::User(queued) => {
                                        queued.timestamp == user.timestamp
                                    }
                                    _ => false,
                                });
                        if let Some(index) = delivered {
                            self.queued_deliveries.remove(index);
                            let _ = self.session.append_message(message.clone());
                            self.update_pending_messages();
                        }
                    }
                }
                AgentMessage::Assistant(assistant) => {
                    if let Some(index) = self.streaming_index.take()
                        && let Some(component) =
                            self.downcast_chat::<AssistantMessageComponent>(index)
                    {
                        component.update_content(assistant, false);
                    }
                    let aborted = assistant.stop_reason == pi_ai::types::StopReason::Aborted
                        || assistant.stop_reason == pi_ai::types::StopReason::Error;
                    let ids: Vec<String> = self.pending_tools.keys().cloned().collect();
                    for id in ids {
                        let Some(pending) = self.pending_tools.get(&id) else {
                            continue;
                        };
                        let index = pending.chat_index;
                        if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(index)
                        {
                            if aborted {
                                let error = assistant
                                    .error_message
                                    .clone()
                                    .unwrap_or_else(|| "Error".to_string());
                                component.update_result(
                                    ToolResultView {
                                        text: error,
                                        is_error: true,
                                        duration_ms: None,
                                    },
                                    false,
                                );
                            } else {
                                component.set_args_complete();
                            }
                        }
                    }
                    if aborted {
                        self.pending_tools.clear();
                    }
                    let _ = self.session.append_message(message.clone());
                    self.refresh_footer();
                    self.tui.request_render();
                }
                AgentMessage::ToolResult(result) => {
                    if let Some(pending) = self.pending_tools.remove(&result.tool_call_id) {
                        let index = pending.chat_index;
                        let blocks: Vec<pi_ai::types::ContentBlock> = result
                            .content
                            .iter()
                            .cloned()
                            .map(pi_ai::types::ContentBlock::from)
                            .collect();
                        let text = crate::tools::render_utils::get_text_output(&blocks, true);
                        if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(index)
                        {
                            component.update_result(
                                ToolResultView {
                                    text,
                                    is_error: result.is_error,
                                    duration_ms: result.duration_ms,
                                },
                                false,
                            );
                        }
                    }
                    let _ = self.session.append_message(message.clone());
                    self.refresh_footer();
                    self.tui.request_render();
                }
                other => {
                    let _ = self.session.append_message(other.clone());
                }
            },
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                if let Some(pending) = self.pending_tools.get(tool_call_id) {
                    let index = pending.chat_index;
                    if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(index) {
                        component.mark_execution_started();
                        component.note_started();
                    }
                }
                self.tui.request_render();
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                if let Some(pending) = self.pending_tools.get(tool_call_id) {
                    let index = pending.chat_index;
                    let text =
                        crate::tools::render_utils::get_text_output(&partial_result.content, true);
                    if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(index) {
                        component.update_result(
                            ToolResultView {
                                text,
                                is_error: false,
                                duration_ms: None,
                            },
                            true,
                        );
                    }
                }
                self.tui.request_render();
            }
            AgentEvent::AgentEnd { .. } => {
                self.refresh_footer();
                self.tui.request_render();
            }
        }
    }

    /// Recompute the footer's usage totals from the session
    /// (`FooterComponent.getSessionStats`).
    fn refresh_footer(&mut self) {
        let mut totals = UsageTotals::default();
        let mut latest_cache_hit_rate: Option<f64> = None;
        let mut context_percent: Option<f64> = None;
        for entry in self.session.get_entries() {
            let crate::session::records::SessionEntry::Message(entry) = &entry else {
                continue;
            };
            let AgentMessage::Assistant(assistant) = &entry.message else {
                continue;
            };
            totals.add(&assistant.usage);
            let prompt_tokens =
                assistant.usage.input + assistant.usage.cache_read + assistant.usage.cache_write;
            if prompt_tokens > 0 {
                latest_cache_hit_rate =
                    Some(assistant.usage.cache_read as f64 / prompt_tokens as f64 * 100.0);
                context_percent =
                    Some(prompt_tokens as f64 / self.model.context_window.max(1) as f64 * 100.0);
            }
        }
        let thinking_level = self.thinking_level.as_str().to_string();
        let model_id = self.model.id.clone();
        let model_supports_reasoning = self.model.reasoning;
        let Some(footer) = self.downcast_tree::<FooterComponent>(HostSlot::Footer) else {
            return;
        };
        footer.data.totals = totals;
        footer.data.latest_cache_hit_rate = latest_cache_hit_rate;
        footer.data.context_percent = context_percent;
        footer.data.thinking_level = thinking_level;
        footer.data.model_id = model_id;
        footer.data.model_supports_reasoning = model_supports_reasoning;
    }

    // --------------------------------------------------------------
    // Easter eggs (`handleArminSaysHi`, `interactive-mode.ts:5993`)
    // --------------------------------------------------------------

    /// `playEasterEgg3d`'s lazy wrapper (`easter-egg-3d.lazy.js`): only
    /// fullscreen can show it, because it dissolves the rendered screen, and
    /// the screen is captured first. Returns false when it cannot play so the
    /// caller can fall back, and true — absorbed, as upstream — when an
    /// overlay is already showing.
    fn play_easter_egg(&mut self, egg: EasterEgg3d) -> bool {
        if !self.tui.alt_screen_active() {
            return false;
        }
        if self.tui.has_overlay() {
            return true;
        }
        let screen = self.tui.last_lines().to_vec();
        self.easter_egg = Some(play_easter_egg_3d(&mut self.tui, screen, egg));
        true
    }

    /// The 3D Armin when it can play, otherwise the inline effect appended
    /// to the chat.
    fn handle_armin_says_hi(&mut self) {
        if self.play_easter_egg(EasterEgg3d::Armin) {
            return;
        }
        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        self.add_to_chat(Box::new(ArminComponent::new()));
        self.tui.request_render();
    }

    // --------------------------------------------------------------
    // Submission (`setupEditorSubmitHandler`, `interactive-mode.ts:3324`)
    // --------------------------------------------------------------

    fn submit(&mut self, text: &str) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if text == "/new" || text == "/clear" {
            self.handle_clear();
            return;
        }
        if text == "/exit" || text == "/quit" {
            self.exiting = true;
            return;
        }
        // `/arminsayshi` (`interactive-mode.ts:2773-2777`): the hidden
        // easter egg command — 3D when it can play, inline otherwise.
        if text == "/arminsayshi" {
            self.handle_armin_says_hi();
            return;
        }
        if let Some(rest) = text.strip_prefix("/model") {
            self.handle_model_command(rest.trim());
            return;
        }
        // `/trust` (`interactive-mode.ts:2736-2739`).
        if text == "/trust" {
            self.open_trust_selector();
            return;
        }
        // `/settings` (`interactive-mode.ts:2653-2655`).
        if text == "/settings" {
            self.open_settings_selector();
            return;
        }
        // `/resume` (`interactive-mode.ts:2783-2786`).
        if text == "/resume" {
            self.open_session_selector();
            return;
        }
        // `/compact [instructions]` (`interactive-mode.ts:3434-3439`).
        if text == "/compact" || text.starts_with("/compact ") {
            let custom = text
                .strip_prefix("/compact ")
                .map(str::trim)
                .filter(|s| !s.is_empty());
            self.handle_compact_command(custom);
            return;
        }
        // `/copy` (`interactive-mode.ts:3372-3376`).
        if text == "/copy" {
            self.handle_copy_command();
            return;
        }
        // `/name [name]` (`interactive-mode.ts:3377-3381`).
        if text == "/name" || text.starts_with("/name ") {
            self.handle_name_command(&text);
            return;
        }
        // `/session` (`interactive-mode.ts:3382-3386`).
        if text == "/session" {
            self.handle_session_command();
            return;
        }
        // `/changelog` (`interactive-mode.ts:3387-3391`).
        if text == "/changelog" {
            self.handle_changelog_command();
            return;
        }
        // `/hotkeys` (`interactive-mode.ts:3392-3396`).
        if text == "/hotkeys" {
            self.handle_hotkeys_command();
            return;
        }
        // `/login [provider]` (`interactive-mode.ts:3418-3423`): an empty
        // reference rides as `""`, which matches nothing and opens the
        // provider selector (`findMatchingProviders`'s normalized-empty
        // guard, `provider-matching.ts:8-10`).
        if text == "/login" || text.starts_with("/login ") {
            let provider_ref = text.strip_prefix("/login ").map(str::trim);
            self.handle_login_command(provider_ref);
            return;
        }
        // `/logout` (`interactive-mode.ts:3424-3426`).
        if text == "/logout" {
            self.show_oauth_selector("logout");
            return;
        }
        if let Some(rest) = text.strip_prefix("/thinking") {
            self.handle_thinking_command(rest.trim());
            return;
        }
        if let Some(rest) = text.strip_prefix("/theme") {
            self.handle_theme_command(rest.trim());
            return;
        }
        // `!` bash mode (`interactive-mode.ts:3471-3487`).
        if let Some(rest) = text.strip_prefix('!') {
            let excluded = rest.starts_with('!');
            let command = rest.trim_start_matches('!').trim().to_string();
            if !command.is_empty() {
                self.run_bash(&command, excluded);
                return;
            }
        }
        // Queue input during compaction (`interactive-mode.ts:3489-3499`):
        // built-in commands already dispatched above.
        if self.compaction_run.is_some() {
            self.queue_compaction_message(&text, false);
            return;
        }
        if self.agent_run.is_some() {
            // Upstream Enter during streaming steers
            // (`interactive-mode.ts:3500-3509`, `streamingBehavior: "steer"`).
            self.queue_message(&text, false);
            return;
        }
        self.start_agent_run(&text);
    }

    /// Queue a user message for the running agent (`session.prompt` with
    /// `streamingBehavior`, `agent-session.ts:2240-2253`). The message rides
    /// the loop's queue, shows in the pending display, and persists when the
    /// loop declares it — so the transcript order matches upstream.
    fn queue_message(&mut self, text: &str, follow_up: bool) {
        self.editor_mut().editor.add_to_history(text);
        let message = pi_agent::transcript::user_message(text, now_millis());
        self.queued_deliveries.push((follow_up, message.clone()));
        {
            let mut queues = self.queues.lock().expect("queues");
            if follow_up {
                queues.follow_up(std::iter::once(message));
            } else {
                queues.steer(std::iter::once(message));
            }
        }
        self.update_pending_messages();
        self.tui.request_render();
    }

    // --------------------------------------------------------------
    // Compaction (`session.compact`, `agent-session.ts:2936`)
    // --------------------------------------------------------------

    /// `/compact` (`handleCompactCommand`, `interactive-mode.ts:7273-7281`):
    /// abort the current run first (manual compaction never retries it),
    /// then prepare + summarize on a worker thread. The session append
    /// happens on the UI thread when the outcome lands, like upstream's
    /// `compaction_end` handling.
    #[allow(clippy::too_many_lines)] // mirrors the TS compaction flow incl. virtual routing; split when extending
    fn handle_compact_command(&mut self, custom_instructions: Option<&str>) {
        if self.compaction_run.is_some() {
            return;
        }
        self.abort_active_run();

        // Settings-driven tuning lands with a settings handle in this mode;
        // `getCompactionSettings` itself is ported and tested.
        let settings = crate::session::compaction::DEFAULT_COMPACTION_SETTINGS;

        let path_entries = self.session.get_branch(None).unwrap_or_default();
        let already_compacted = matches!(
            path_entries.last(),
            Some(crate::session::records::SessionEntry::Compaction(_))
        );
        let Some(preparation) =
            crate::session::compaction::prepare_compaction(&path_entries, settings)
        else {
            // Check why we can't compact, like upstream (`:2962-2968`).
            self.show_error(&format!(
                "Compaction failed: {}",
                if already_compacted {
                    "Already compacted"
                } else {
                    "Nothing to compact (session too small)"
                }
            ));
            return;
        };

        // `session_before_compact` (`agent-session.ts:2964-2982`): the
        // manual gate carries the preparation, the branch entries (the raw
        // JSONL records, key order included), and the custom instructions;
        // a `cancel` vote aborts. Extension-provided compaction results are
        // not plumbed yet (§8 of docs/EXTENSIONS.md).
        if self.extension_cancelled(&serde_json::json!({
            "type": "session_before_compact",
            "preparation": serde_json::to_value(&preparation)
                .unwrap_or(serde_json::Value::Null),
            "branchEntries": serde_json::Value::Array(
                self.session
                    .get_branch_records(None)
                    .iter()
                    .map(|record| serde_json::Value::Object(record.object().clone()))
                    .collect(),
            ),
            "customInstructions": custom_instructions,
            "reason": "manual",
            "willRetry": false,
        })) {
            self.show_error("Compaction cancelled");
            return;
        }

        let model = self.model.clone();
        // Route a virtual selection first: summaries size their input and
        // output from the model they get (`_getSummarizationRequestAuth`,
        // agent-session.ts:617-624; reason "direct", no router state).
        #[cfg(feature = "ext-js")]
        let (model, api_key, access_token, thinking_level) =
            if crate::virtual_models::is_virtual_model(&model) {
                let Some(runner) = &self.extension_runner else {
                    self.show_error(&format!(
                        "Virtual model {}/{} is not registered.",
                        model.provider, model.id
                    ));
                    return;
                };
                let messages = self
                    .session
                    .build_session_projection()
                    .map(|projection| projection.messages)
                    .unwrap_or_default();
                match crate::virtual_models::resolve_virtual_model(
                    runner,
                    &crate::virtual_models::RouteRequest {
                        model: &model,
                        messages: &messages,
                        reason: crate::virtual_models::ModelRouteReason::Direct,
                        thinking_level: self.thinking_level,
                        state: None,
                        failed: None,
                    },
                    crate::virtual_models::RouteResolvers {
                        get_model: &|provider, id| crate::models_state::lookup(provider, id),
                        has_credentials: &|provider| {
                            crate::modes::print_mode::resolve_wire_credential(
                                provider,
                                None,
                                &self.auth_path,
                            )
                            .is_some()
                        },
                    },
                ) {
                    Ok(route) => {
                        // The routed provider's own credential decides the
                        // summary request (the flag key stays scoped to the
                        // selected provider).
                        let credential = crate::modes::print_mode::resolve_wire_credential(
                            route.model.provider.as_str(),
                            if route.model.provider == self.model.provider {
                                self.api_key.as_deref()
                            } else {
                                None
                            },
                            &self.auth_path,
                        );
                        let (api_key, access_token) =
                            credential.map_or((None, None), |(credential, _)| match credential {
                                crate::modes::print_mode::WireCredential::ApiKey(key) => {
                                    (Some(key), None)
                                }
                                crate::modes::print_mode::WireCredential::AccessToken(token) => {
                                    (None, Some(token))
                                }
                            });
                        (route.model, api_key, access_token, route.thinking_level)
                    }
                    Err(error) => {
                        self.show_error(&error);
                        return;
                    }
                }
            } else {
                (
                    model,
                    self.api_key.clone(),
                    self.access_token.clone(),
                    self.thinking_level,
                )
            };
        #[cfg(not(feature = "ext-js"))]
        let (model, api_key, access_token, thinking_level) = (
            model,
            self.api_key.clone(),
            self.access_token.clone(),
            self.thinking_level,
        );
        let stream_fn = Arc::clone(&self.stream_fn);
        let signal = Arc::new(AbortSignal::new());
        let (tx, rx) = std::sync::mpsc::channel::<CompactionOutcome>();
        let run_signal = Arc::clone(&signal);
        let custom = custom_instructions.map(str::to_string);
        let handle = std::thread::spawn(move || {
            let outcome = match crate::runtime::block_on(crate::session::compaction::compact(
                &preparation,
                &model,
                api_key.as_deref(),
                access_token.as_deref(),
                custom.as_deref(),
                &run_signal,
                thinking_level,
                stream_fn.as_ref(),
            )) {
                Ok(result) => CompactionOutcome::Completed(Box::new(result)),
                Err(crate::session::compaction::CompactionError::Cancelled) => {
                    CompactionOutcome::Cancelled
                }
                Err(crate::session::compaction::CompactionError::Failed(message)) => {
                    CompactionOutcome::Failed(message)
                }
            };
            let _ = tx.send(outcome);
        });
        self.compaction_run = Some(CompactionRun {
            abort: signal,
            events: rx,
            handle: Some(handle),
        });
        self.set_working_status(Some(StatusIndicator::compaction(
            "Compacting context... (ctrl+c to cancel)",
        )));
        self.tui.request_render();
    }

    /// Abort the in-flight agent run and settle it: join the thread, then
    /// persist whatever the loop already emitted (upstream `await
    /// this.abort()` inside `session.compact`).
    fn abort_active_run(&mut self) {
        let Some(run) = self.agent_run.take() else {
            return;
        };
        run.abort.abort();
        if let Some(handle) = run.handle {
            let _ = handle.join();
        }
        for event in run.events.try_iter() {
            if let LoopEvent::Event(event) = event {
                self.handle_agent_event(&event);
            }
        }
        self.set_working_status(None);
    }

    /// `compaction_end` handling (`interactive-mode.ts:3820-3855`): persist
    /// the summary, render the rows, flush the queue.
    fn handle_compaction_outcome(&mut self, outcome: CompactionOutcome) {
        match outcome {
            CompactionOutcome::Completed(result) => {
                let details = crate::session::compaction::details_json(&result);
                if let Err(error) = self.session.append_compaction(
                    &result.summary,
                    Some(&result.first_kept_entry_id),
                    result.tokens_before,
                    Some(details),
                    Some(false),
                    result.usage,
                ) {
                    self.show_error(&format!("Compaction failed: {error}"));
                } else {
                    self.add_compaction_rows(&result);
                    self.refresh_footer();
                }
            }
            CompactionOutcome::Failed(message) => {
                self.show_error(&format!("Compaction failed: {message}"));
            }
            CompactionOutcome::Cancelled => self.show_status("Compaction cancelled"),
        }
        self.flush_compaction_queue();
    }

    /// The summary row + the billed-tokens notice (`addCompactionCostNotice`,
    /// `interactive-mode.ts:4305-4319`; its `showCacheMissNotices` gate is
    /// not ported yet, so the notice always shows — documented deviation).
    /// Deviation: the chat keeps its rendered history (upstream re-renders
    /// and drops the summarized-away rows; that rebuild lands with the
    /// `renderSessionEntries` port).
    fn add_compaction_rows(&mut self, result: &crate::session::compaction::CompactionResult) {
        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        let mut row = super::components::compaction_summary::CompactionSummaryMessageComponent::new(
            &result.summary,
            result.tokens_before,
            1,
        );
        row.set_expanded(self.tool_output_expanded);
        self.add_to_chat(Box::new(row));
        if let Some(usage) = &result.usage {
            let tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
            let cost = if usage.cost.total >= 0.01 {
                format!(" (~${:.2})", usage.cost.total)
            } else {
                String::new()
            };
            self.add_to_chat(Box::new(Text::new(
                theme::theme().fg(
                    pi_tui::theme::ThemeColor::Warning,
                    &format!(
                        "Compaction: {} tokens billed{cost}",
                        super::components::footer::format_tokens(tokens)
                    ),
                ),
                1,
                0,
            )));
        }
    }

    /// `queueCompactionMessage` (`interactive-mode.ts:4951-4958`).
    fn queue_compaction_message(&mut self, text: &str, follow_up: bool) {
        self.compaction_queued.push((text.to_string(), follow_up));
        self.editor_mut().editor.add_to_history(text);
        self.editor_mut().editor.set_text("");
        self.update_pending_messages();
        self.show_status("Queued message for after compaction");
    }

    /// `flushCompactionQueue` (`interactive-mode.ts:4958-5000`): after a
    /// compaction ends, the first queued message starts a run and the rest
    /// ride the steering/follow-up queues.
    fn flush_compaction_queue(&mut self) {
        if self.compaction_queued.is_empty() {
            return;
        }
        let queued = std::mem::take(&mut self.compaction_queued);
        self.update_pending_messages();
        let mut rest = queued.into_iter();
        if let Some((first, _)) = rest.next() {
            self.submit(&first);
            for (text, follow_up) in rest {
                self.queue_message(&text, follow_up);
            }
        }
    }

    /// `handleDequeue` (`interactive-mode.ts:4691-4699`) via
    /// `restoreQueuedMessagesToEditor`: put every queued message back into
    /// the editor, newest queue order first.
    fn dequeue_queued(&mut self) {
        let queued_texts: Vec<String> = self
            .queued_deliveries
            .iter()
            .filter_map(|(_, message)| match message {
                AgentMessage::User(user) => match &user.content {
                    pi_ai::types::UserContent::Text(text) => Some(text.clone()),
                    pi_ai::types::UserContent::Blocks(_) => None,
                },
                _ => None,
            })
            .collect();
        if queued_texts.is_empty() {
            self.show_status("No queued messages to restore");
            return;
        }
        self.queues.lock().expect("queues").clear_all();
        self.queued_deliveries.clear();
        let current_text = self.editor_mut().editor.get_text();
        let combined = queued_texts
            .into_iter()
            .chain(std::iter::once(current_text))
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.editor_mut().editor.set_text(&combined);
        self.update_pending_messages();
        self.tui.request_render();
    }

    /// `updatePendingMessagesDisplay` (`interactive-mode.ts:4887-4909`).
    fn update_pending_messages(&mut self) {
        // Build the rows first: the container borrow must not overlap the
        // queue bookkeeping.
        let theme = theme::theme();
        let mut rows: Vec<Box<dyn Component>> = Vec::new();
        if !self.queued_deliveries.is_empty() || !self.compaction_queued.is_empty() {
            rows.push(Box::new(pi_tui::components::spacer::Spacer::new(1)));
            for (follow_up, message) in &self.queued_deliveries {
                let AgentMessage::User(user) = message else {
                    continue;
                };
                let text = match &user.content {
                    pi_ai::types::UserContent::Text(text) => text.clone(),
                    pi_ai::types::UserContent::Blocks(_) => continue,
                };
                let label = if *follow_up { "Follow-up" } else { "Steering" };
                rows.push(Box::new(
                    pi_tui::components::truncated_text::TruncatedText::new(
                        theme.fg(pi_tui::theme::ThemeColor::Dim, &format!("{label}: {text}")),
                        1,
                        0,
                    ),
                ));
            }
            for (text, follow_up) in &self.compaction_queued {
                let label = if *follow_up { "Follow-up" } else { "Steering" };
                rows.push(Box::new(
                    pi_tui::components::truncated_text::TruncatedText::new(
                        theme.fg(pi_tui::theme::ThemeColor::Dim, &format!("{label}: {text}")),
                        1,
                        0,
                    ),
                ));
            }
            let dequeue_hint = theme::key_hint(
                theme::app_keys::MESSAGE_DEQUEUE,
                "to edit all queued messages",
            );
            rows.push(Box::new(
                pi_tui::components::truncated_text::TruncatedText::new(
                    theme.fg(pi_tui::theme::ThemeColor::Dim, &format!("↳ {dequeue_hint}")),
                    1,
                    0,
                ),
            ));
        }
        let pending = self
            .downcast_tree::<pi_tui::component::Container>(HostSlot::Pending)
            .expect("pending container");
        pending.clear();
        for row in rows {
            pending.add_child(row);
        }
    }

    /// `/model [pattern]` (`handleModelCommand`, `interactive-mode.ts:5386-5407`):
    /// no pattern opens the selector; an exact match switches; anything else
    /// opens the selector pre-filled with the search term.
    fn handle_model_command(&mut self, pattern: &str) {
        if pattern.is_empty() {
            self.open_model_selector(None);
            return;
        }
        let parsed = crate::modes::print_mode::parse_model_pattern(pattern);
        match crate::modes::print_mode::resolve_model_with(
            Some(&parsed),
            None,
            None,
            &self.virtual_catalog_models(),
        ) {
            Ok(model) => match self.apply_model(model) {
                Ok(()) => self.show_status(&format!("Model: {}", self.model.id)),
                Err(error) => self.show_error(&error),
            },
            Err(_) => self.open_model_selector(Some(pattern)),
        }
    }

    /// The extension-registered virtual models as catalog entries
    /// (`withVirtualModels`' additions to a provider's models).
    #[cfg(feature = "ext-js")]
    fn virtual_catalog_models(&self) -> Vec<pi_ai::types::Model> {
        // Registrations apply live (`model-runtime.ts`); rebuild the
        // composed snapshot when the registry moved.
        if let Some(runner) = &self.extension_runner {
            crate::models_state::refresh_with_runner(
                &runner.lock().expect("extension runner mutex poisoned"),
            );
        }
        self.extension_runner
            .as_ref()
            .map(|runner| {
                crate::virtual_models::virtual_models_from_defs(
                    &runner
                        .lock()
                        .expect("extension runner mutex poisoned")
                        .virtual_models(),
                )
            })
            .unwrap_or_default()
    }

    /// Engine-free shape: no extension host, no virtual registrations.
    #[cfg(not(feature = "ext-js"))]
    fn virtual_catalog_models(&self) -> Vec<pi_ai::types::Model> {
        Vec::new()
    }

    /// Swap the active model, including the provider adapter when the api
    /// changes (`handleModelCommand` and `session.cycleModel` share the path).
    /// A virtual selection swaps in the routed-dispatch stream function: the
    /// per-request model decides the adapter (`withVirtualModels`' stream).
    /// A physical selection binds one adapter — wrapped in the composed
    /// dispatch when the engine exists, so a provider's registered
    /// `streamSimple` handler wins per request (`streamWith`,
    /// provider-composer.ts:588-613).
    fn apply_model(&mut self, model: pi_ai::types::Model) -> Result<(), String> {
        if crate::virtual_models::is_virtual_model(&model) {
            self.stream_fn = Arc::new(crate::virtual_models::RoutedStreamFn {
                selected_provider: model.provider.clone(),
                flag_api_key: self.api_key.clone(),
                auth_path: self.auth_path.clone(),
                route_error: Arc::new(crate::virtual_models::RouteErrorSlot::default()),
                #[cfg(feature = "ext-js")]
                runner: self.extension_runner.clone(),
            });
        } else if model.api != self.model.api {
            let Some(stream_fn) = pi_ai::providers::stream_fn_for(&model.api) else {
                return Err(format!(
                    "No provider adapter for api \"{}\" (model {}/{}).",
                    model.api, model.provider, model.id
                ));
            };
            let base: Arc<dyn pi_agent::types::StreamFn> =
                Arc::new(crate::modes::print_mode::ProviderStreamFn(stream_fn));
            #[cfg(feature = "ext-js")]
            let base = match &self.extension_runner {
                Some(runner) => Arc::new(crate::models_state::ComposedStreamFn {
                    runner: Arc::clone(runner),
                    fallback: base,
                }) as Arc<dyn pi_agent::types::StreamFn>,
                None => base,
            };
            self.stream_fn = base;
        }
        let changed = model.provider != self.model.provider || model.id != self.model.id;
        self.model = model;
        if changed {
            let _ = self
                .session
                .append_model_change(&self.model.provider, &self.model.id);
        }
        self.refresh_footer();
        self.tui.request_render();
        Ok(())
    }

    /// `modelRuntime.getAvailableSnapshot()` — the cycle source
    /// (`agent-session.ts` `_cycleAvailableModel`). Upstream auth-filters
    /// through provider `checkAuth`; the port reuses the request-time
    /// credential ladder (stored `auth.json` entry or provider env key),
    /// with the `--api-key` flag counting for the initial provider only.
    /// Virtual models ride their provider unconditionally (upstream's
    /// `withVirtualModels` appends them outside `filterModels`), and hide a
    /// same-id physical model.
    fn available_models(&self) -> Vec<pi_ai::types::Model> {
        let auth_path = &self.auth_path;
        let composed = crate::models_state::all();
        let virtual_models = self.virtual_catalog_models();
        let mut models = Vec::new();
        for provider in crate::models_state::providers() {
            let flag_key = if provider == self.model.provider {
                self.api_key.clone()
            } else {
                None
            };
            if crate::modes::print_mode::resolve_wire_credential(
                &provider,
                flag_key.as_deref(),
                auth_path,
            )
            .is_some()
            {
                models.extend(
                    composed
                        .iter()
                        .filter(|model| model.provider == provider)
                        .cloned(),
                );
            }
        }
        crate::virtual_models::with_virtual_model_listing(models, &virtual_models)
    }

    /// ctrl+p / ctrl+shift+p cycling (`cycleModel`, `interactive-mode.ts:4722-4741`).
    /// Upstream also applies per-model thinking defaults on the switch; those
    /// land with the settings work.
    fn cycle_model(&mut self, direction: i32) {
        let available = self.available_models();
        let Some(next) = cycle_next(&available, &self.model.provider, &self.model.id, direction)
        else {
            self.show_status("Only one model available");
            return;
        };
        if let Err(error) = self.apply_model(next) {
            self.show_error(&error);
            return;
        }
        // `Switched to ${model.name || model.id}` + the thinking suffix
        // (`interactive-mode.ts:4731-4735`).
        let label = if self.model.name.is_empty() {
            self.model.id.clone()
        } else {
            self.model.name.clone()
        };
        let thinking_suffix = if self.model.reasoning && self.thinking_level != ThinkingLevel::Off {
            format!(" (thinking: {})", self.thinking_level.as_str())
        } else {
            String::new()
        };
        self.show_status(&format!("Switched to {label}{thinking_suffix}"));
    }

    /// Mount the `/model` selector in the editor's tree slot and focus it
    /// (`showSelector`, `interactive-mode.ts:5072-5096`).
    fn open_model_selector(&mut self, initial_search: Option<&str>) {
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = ModelSelectorComponent::new(
            &self.model,
            self.available_models(),
            initial_search,
            Arc::clone(&events),
        );
        self.model_selector_events = Some(events);
        self.suspend_editor_with(Box::new(selector));
    }

    /// Put the editor back (`showSelector`'s `done`).
    fn close_model_selector(&mut self) {
        if self.suspended_editor.is_none() {
            return;
        }
        self.model_selector_events = None;
        self.restore_editor();
    }

    /// Mount the `/trust` selector in the editor's tree slot
    /// (`showTrustSelector`, `interactive-mode.ts:4567-4589`).
    fn open_trust_selector(&mut self) {
        if self.suspended_editor.is_some() {
            return;
        }
        let cwd = self.session.get_cwd().to_path_buf();
        let store = ProjectTrustStore::new(&self.agent_dir);
        let saved_decision = store.get_entry(&cwd).ok().flatten();
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = TrustSelectorComponent::new(
            &cwd,
            saved_decision,
            self.project_trusted,
            Arc::clone(&events),
        );
        self.trust_selector_events = Some(events);
        self.suspend_editor_with(Box::new(selector));
    }

    /// Put the editor back after a trust decision or a cancel.
    fn close_trust_selector(&mut self) {
        if self.suspended_editor.is_none() {
            return;
        }
        self.trust_selector_events = None;
        self.restore_editor();
    }

    /// Mount the `/settings` dialog in the editor's tree slot
    /// (`showSettingsSelector`, `interactive-mode.ts:4178-4416`).
    fn open_settings_selector(&mut self) {
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let (provider, model_id) = (
            self.settings.get_default_provider(),
            self.settings.get_default_model(),
        );
        let default_model = provider.zip(model_id);
        let selector = SettingsSelectorComponent::new(
            &SettingsSelectorConfig {
                steering_mode: queue_mode_as_setting(
                    self.queues.lock().expect("queues").steering_mode(),
                )
                .to_string(),
                follow_up_mode: queue_mode_as_setting(
                    self.queues.lock().expect("queues").follow_up_mode(),
                )
                .to_string(),
                hide_thinking_block: self.hide_thinking_block,
                thinking_level: self
                    .settings
                    .get_default_thinking_level()
                    .unwrap_or_else(|| "off".to_string()),
                model_thinking_levels: model_thinking_levels(&self.settings),
                available_models: self.available_models(),
                current_model: Some((self.model.provider.clone(), self.model.id.clone())),
                default_model,
                current_theme: self
                    .theme_setting
                    .clone()
                    .unwrap_or_else(|| pi_tui::theme::SYSTEM_THEME_NAME.to_string()),
                terminal_theme: theme::terminal_theme().to_string(),
                default_project_trust: self
                    .settings
                    .get("defaultProjectTrust")
                    .and_then(|value| value.as_str())
                    .unwrap_or("ask")
                    .to_string(),
                tui_mode: tui_mode_as_setting(self.settings.get_tui_mode()).to_string(),
                fullscreen_exit_output: match self.settings.get_fullscreen_exit_output() {
                    FullscreenExitOutput::Transcript => "transcript",
                    FullscreenExitOutput::ResumeHint => "resume-hint",
                }
                .to_string(),
                fullscreen_scrollbar: match self.settings.get_fullscreen_scrollbar() {
                    FullscreenScrollbar::Auto => "auto",
                    FullscreenScrollbar::Always => "always",
                    FullscreenScrollbar::Hidden => "hidden",
                }
                .to_string(),
                fullscreen_copy_on_select: self.settings.get_fullscreen_copy_on_select(),
                fullscreen_wheel_scroll_lines: wheel_lines_as_setting(
                    self.settings.get_fullscreen_wheel_scroll_lines(),
                ),
            },
            Arc::clone(&events),
        );
        self.settings_selector_events = Some(events);
        self.suspend_editor_with(Box::new(selector));
    }

    /// Put the editor back after the settings dialog closes.
    fn close_settings_selector(&mut self) {
        if self.suspended_editor.is_none() {
            return;
        }
        self.settings_selector_events = None;
        self.restore_editor();
    }

    /// The `/settings` dialog's event handlers
    /// (`showSettingsSelector`'s callbacks, `interactive-mode.ts:4227-4412`).
    fn handle_settings_event(&mut self, event: SettingsSelectorEvent) {
        match event {
            SettingsSelectorEvent::TuiModeChange(_)
            | SettingsSelectorEvent::FullscreenExitOutputChange(_)
            | SettingsSelectorEvent::FullscreenScrollbarChange(_)
            | SettingsSelectorEvent::FullscreenCopyOnSelectChange(_)
            | SettingsSelectorEvent::FullscreenWheelScrollLinesChange(_) => {
                self.handle_fullscreen_settings_event(event);
            }
            SettingsSelectorEvent::ThemeChange(setting) => {
                // `onThemeChange` (`interactive-mode.ts:4301-4304`):
                // persist, then re-apply through the controller.
                self.settings
                    .set("theme", serde_json::Value::String(setting.clone()));
                self.theme_setting = Some(setting);
                let _ = self.theme_apply_from_settings(true);
            }
            SettingsSelectorEvent::ThemePreview(setting) => {
                // `onThemePreview` → `preview`
                // (`theme-controller.ts:111-119`): switch the loaded theme
                // without recording the setting.
                let name = theme::resolve_theme_setting(Some(&setting))
                    .unwrap_or_else(|| self.theme_active_name.clone());
                let _ = theme::set_theme(&name);
                self.tui.invalidate();
                self.tui.request_render();
            }
            SettingsSelectorEvent::SteeringModeChange(mode) => {
                // `onSteeringModeChange` → `session.setSteeringMode`
                // (`agent-session.ts:2286-2289`): the live queues and
                // settings.json.
                self.queues
                    .lock()
                    .expect("queues")
                    .set_steering_mode(queue_mode_from_setting(Some(&mode)));
                self.settings
                    .set("steeringMode", serde_json::Value::String(mode));
            }
            SettingsSelectorEvent::FollowUpModeChange(mode) => {
                self.queues
                    .lock()
                    .expect("queues")
                    .set_follow_up_mode(queue_mode_from_setting(Some(&mode)));
                self.settings
                    .set("followUpMode", serde_json::Value::String(mode));
            }
            SettingsSelectorEvent::HideThinkingBlockChange(hidden) => {
                // `onHideThinkingBlockChange`
                // (`interactive-mode.ts:4306-4310`).
                self.hide_thinking_block = hidden;
                self.settings
                    .set("hideThinkingBlock", serde_json::Value::Bool(hidden));
                self.update_thinking_block_visibility();
                self.tui.request_render();
            }
            SettingsSelectorEvent::ModelThinkingLevelChange {
                provider,
                model_id,
                level,
            } => {
                self.apply_model_thinking_level(&provider, &model_id, Some(&level));
            }
            SettingsSelectorEvent::ModelThinkingLevelRemove { provider, model_id } => {
                self.apply_model_thinking_level(&provider, &model_id, None);
            }
            SettingsSelectorEvent::DefaultProjectTrustChange(value) => {
                self.settings
                    .set("defaultProjectTrust", serde_json::Value::String(value));
            }
            SettingsSelectorEvent::Cancel => {
                self.close_settings_selector();
            }
        }
    }

    /// The fullscreen rows' handlers (`onTuiModeChange` and the four
    /// fullscreen callbacks, `interactive-mode.ts:4374-4402`).
    fn handle_fullscreen_settings_event(&mut self, event: SettingsSelectorEvent) {
        match event {
            SettingsSelectorEvent::TuiModeChange(mode) => {
                // A selector occupying the editor slot blocks the switch; the
                // row reverts to the current mode and the status says why.
                // On success the setting persists and the mode shows.
                let target = match mode.as_str() {
                    "regular" => TuiMode::Regular,
                    _ => TuiMode::Fullscreen,
                };
                if !self.switch_tui_mode(target) {
                    let current = tui_mode_as_setting(self.tui_mode).to_string();
                    if let Some(selector) =
                        self.downcast_tree::<SettingsSelectorComponent>(HostSlot::Editor)
                    {
                        selector.settings_list().update_value("tui-mode", &current);
                    }
                    self.show_status("Close active overlays before changing TUI mode");
                    return;
                }
                self.settings.set_tui_mode(target);
                self.show_status(&format!("TUI mode: {mode}"));
            }
            SettingsSelectorEvent::FullscreenExitOutputChange(output) => {
                // `onFullscreenExitOutputChange` (`interactive-mode.ts:4386-4388`).
                self.settings
                    .set_fullscreen_exit_output(match output.as_str() {
                        "resume-hint" => FullscreenExitOutput::ResumeHint,
                        _ => FullscreenExitOutput::Transcript,
                    });
            }
            SettingsSelectorEvent::FullscreenScrollbarChange(scrollbar) => {
                // `onFullscreenScrollbarChange` (`interactive-mode.ts:4389-4392`):
                // persist, then re-apply to the live transcript
                // (`applyFullscreenScrollbarSetting`, `:1659-1661`); in
                // regular mode the setting is read at the next mount.
                let parsed = match scrollbar.as_str() {
                    "always" => FullscreenScrollbar::Always,
                    "hidden" => FullscreenScrollbar::Hidden,
                    _ => FullscreenScrollbar::Auto,
                };
                self.settings.set_fullscreen_scrollbar(parsed);
                let transcript_id = self.viewport.as_ref().map(ChatViewport::transcript_id);
                if let Some(transcript_id) = transcript_id
                    && let Some(transcript) = self
                        .tui
                        .component_by_id_deep_mut(transcript_id)
                        .and_then(|component| component.as_any_mut())
                        .and_then(|any| any.downcast_mut::<ScrollView>())
                {
                    transcript.state.set_scrollbar(parsed.into());
                }
            }
            SettingsSelectorEvent::FullscreenCopyOnSelectChange(enabled) => {
                // `onFullscreenCopyOnSelectChange` (`interactive-mode.ts:4393-4397`):
                // persist, then push to the live alt screen.
                self.settings.set_fullscreen_copy_on_select(enabled);
                self.tui.set_copy_on_select(enabled);
            }
            SettingsSelectorEvent::FullscreenWheelScrollLinesChange(lines) => {
                // `onFullscreenWheelScrollLinesChange` (`interactive-mode.ts:4398-4402`):
                // persist, then push to the live alt screen.
                let parsed = fullscreen_wheel_lines(&lines);
                self.settings.set_fullscreen_wheel_scroll_lines(parsed);
                self.tui.set_wheel_scroll_lines(parsed);
            }
            _ => {}
        }
    }

    /// `onModelThinkingLevelChange`/`onModelThinkingLevelRemove`
    /// (`interactive-mode.ts:4280-4300`): persist the override map; when it
    /// concerns the current model, re-apply the thinking level live.
    fn apply_model_thinking_level(&mut self, provider: &str, model_id: &str, level: Option<&str>) {
        let key = format!("{provider}/{model_id}");
        let mut levels = model_thinking_levels(&self.settings);
        match level {
            Some(level) => {
                levels.insert(key, level.to_string());
            }
            None => {
                levels.remove(&key);
            }
        }
        self.settings.set(
            "modelThinkingLevels",
            serde_json::Value::Object(
                levels
                    .into_iter()
                    .map(|(key, value)| (key, serde_json::Value::String(value)))
                    .collect(),
            ),
        );
        if self.model.provider == provider && self.model.id == model_id {
            let default_level = self
                .settings
                .get_default_thinking_level()
                .unwrap_or_else(|| "off".to_string());
            let level = level.unwrap_or(default_level.as_str());
            if let Ok(level) = crate::modes::print_mode::thinking_level_from_str(level) {
                self.thinking_level = level;
                let _ = self.session.append_thinking_level_change(level.as_str());
                self.refresh_footer();
            }
        }
    }

    /// `updateThinkingBlockVisibility`
    /// (`interactive-mode.ts:3887-3891`): flip the flag on every rendered
    /// assistant row.
    fn update_thinking_block_visibility(&mut self) {
        let hide = self.hide_thinking_block;
        let chat_len = self.chat_len();
        for index in 0..chat_len {
            if let Some(message) = self.downcast_chat::<AssistantMessageComponent>(index) {
                message.set_hide_thinking_block(hide);
            }
        }
    }

    /// `/thinking [level]` (`handleThinkingCommand`).
    fn handle_thinking_command(&mut self, level: &str) {
        if level.is_empty() {
            self.show_status(&format!("Thinking level: {}", self.thinking_level.as_str()));
            return;
        }
        match crate::modes::print_mode::thinking_level_from_str(level) {
            Ok(level) => {
                self.thinking_level = level;
                let _ = self.session.append_thinking_level_change(level.as_str());
                self.show_status(&format!("Thinking level: {}", level.as_str()));
                self.refresh_footer();
                self.tui.request_render();
            }
            Err(error) => self.show_error(&error),
        }
    }

    /// `/theme [name]`.
    fn handle_theme_command(&mut self, name: &str) {
        if name.is_empty() {
            self.show_status("Usage: /theme <system|dark|light|light/dark>");
            return;
        }
        // `setThemeSetting` (`theme-controller.ts:111-115`): record the
        // setting, re-resolve against the terminal, re-query. Persistence to
        // settings.json lands with the settings work.
        self.theme_setting = Some(name.to_string());
        if let Ok(()) = self.theme_apply_from_settings(true) {
            self.show_status(&format!("Theme: {name}"));
        }
    }

    // --------------------------------------------------------------
    // Session info commands (`handleNameCommand`, `interactive-mode.ts:6859`)
    // --------------------------------------------------------------

    /// `/name [name]` (`handleNameCommand`, `interactive-mode.ts:6859-6883`):
    /// with a name, persist it as a `session_info` entry and confirm; without
    /// one, show the current name or the usage. The `session_info_changed`
    /// extension event lands with the extension UI wiring.
    fn handle_name_command(&mut self, text: &str) {
        let name = name_argument(text);
        if name.is_empty() {
            match self.session.get_session_name() {
                Some(current_name) => {
                    self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
                    self.add_to_chat(Box::new(Text::new(
                        theme::theme().fg(
                            pi_tui::theme::ThemeColor::Dim,
                            &format!("Session name: {current_name}"),
                        ),
                        1,
                        0,
                    )));
                }
                None => self.show_warning("Usage: /name <name>"),
            }
            self.tui.request_render();
            return;
        }

        // `setSessionName` (`agent-session.ts:4118-4123`) appends the
        // `session_info` entry.
        let _ = self.session.append_session_info(name);
        let session_name = self.session.get_session_name();
        if session_name.as_deref() != Some(name) {
            let normalized = session_name.clone().unwrap_or_default();
            self.show_warning(&format!(
                "Session name was normalized from {} to {}",
                json_stringify(name),
                json_stringify(&normalized),
            ));
        }
        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        let display_name = session_name.unwrap_or_else(|| name.to_string());
        self.add_to_chat(Box::new(Text::new(
            theme::theme().fg(
                pi_tui::theme::ThemeColor::Dim,
                &format!("Session name set: {display_name}"),
            ),
            1,
            0,
        )));
        self.tui.request_render();
    }

    /// `showWarning` (`interactive-mode.ts:4815-4819`): a warning-colored
    /// line in the chat, prefixed `Warning: ` (`interactive-mode.ts:4818`).
    fn show_warning(&mut self, message: &str) {
        self.add_to_chat(Box::new(Text::new(
            theme::theme().fg(
                pi_tui::theme::ThemeColor::Warning,
                &format!("Warning: {message}"),
            ),
            1,
            0,
        )));
        self.tui.request_render();
    }

    /// `/session` (`handleSessionCommand`, `interactive-mode.ts:6884-6963`):
    /// the session info block. Upstream builds the text lazily in a
    /// `ThemedText` so it follows theme changes; the port's `Text` is static
    /// like its other chat lines.
    fn handle_session_command(&mut self) {
        // Snapshot the stats; the text is built on demand so it follows
        // theme changes (upstream, `interactive-mode.ts:6897-6899`).
        let stats = self.session_stats();
        let session_name = self.session.get_session_name();
        let entries = self.session.get_entries();
        // Cost/token totals per provider/model actually used (e.g. OpenRouter
        // `auto` resolves to a concrete responseModel). Usage without model
        // attribution is grouped separately so the breakdown reconciles with
        // the session total (`interactive-mode.ts:6891-6894`).
        let usage_breakdown = super::components::footer::get_usage_cost_breakdown(&entries);
        let cache_waste =
            crate::session::cache_stats::compute_cache_waste(&entries, pi_ai::catalog::catalog());
        let cache_warming_mode = self.cache_warming_mode();
        let selected_model_key = format!("{}/{}", self.model.provider, self.model.id);
        let info = session_info_text(
            &stats,
            session_name.as_deref(),
            &cache_warming_mode,
            &cache_waste,
            &usage_breakdown,
            &selected_model_key,
        );
        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        self.add_to_chat(Box::new(Text::new(info, 1, 0)));
        self.tui.request_render();
    }

    /// `getSessionStats` (`agent-session.ts:4364-4425`): counts over the
    /// session entries plus the usage totals (usage entries, summaries, and
    /// tool results all contribute).
    fn session_stats(&self) -> SessionStats {
        let mut stats = SessionStats {
            session_file: self
                .session
                .get_session_file()
                .map(std::path::Path::to_path_buf),
            session_id: self.session.get_session_id().to_string(),
            ..SessionStats::default()
        };
        for entry in self.session.get_entries() {
            match &entry {
                crate::session::records::SessionEntry::Usage(entry) => {
                    stats.tokens.add(&entry.usage);
                }
                crate::session::records::SessionEntry::BranchSummary(entry) => {
                    if let Some(usage) = &entry.usage {
                        stats.tokens.add(usage);
                    }
                }
                crate::session::records::SessionEntry::Compaction(entry) => {
                    if let Some(usage) = &entry.usage {
                        stats.tokens.add(usage);
                    }
                }
                crate::session::records::SessionEntry::Message(entry) => {
                    stats.total_messages += 1;
                    match &entry.message {
                        AgentMessage::User(_) => stats.user_messages += 1,
                        AgentMessage::ToolResult(message) => {
                            stats.tool_results += 1;
                            if let Some(usage) = &message.usage {
                                stats.tokens.add(usage);
                            }
                        }
                        AgentMessage::Assistant(message) => {
                            stats.assistant_messages += 1;
                            stats.tool_calls += message
                                .content
                                .iter()
                                .filter(|block| {
                                    matches!(block, pi_ai::types::AssistantContent::ToolCall(_))
                                })
                                .count() as u64;
                            stats.tokens.add(&message.usage);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        stats
    }

    /// `getCacheWarmingMode` (`settings-manager.ts:942-945`): the global
    /// `cacheWarming` setting when valid, else the default (`idle`).
    fn cache_warming_mode(&self) -> String {
        const MODES: [&str; 3] = ["off", "streaming", "idle"];
        self.settings
            .get_global_settings()
            .get("cacheWarming")
            .and_then(|value| value.as_str())
            .filter(|mode| MODES.contains(mode))
            .unwrap_or("idle")
            .to_string()
    }

    /// `/hotkeys` (`handleHotkeysCommand`, `interactive-mode.ts:6999-7112`):
    /// every keyboard shortcut, bordered, as markdown.
    fn handle_hotkeys_command(&mut self) {
        let hotkeys = hotkeys_markdown();
        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        self.add_to_chat(Box::new(super::components::header::DynamicBorder::new()));
        self.add_to_chat(Box::new(Text::new(
            theme::theme().fg(
                pi_tui::theme::ThemeColor::Accent,
                &bold_text("Keyboard Shortcuts"),
            ),
            1,
            0,
        )));
        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        self.add_to_chat(Box::new(pi_tui::components::markdown::Markdown::new(
            hotkeys.trim().to_string(),
            1,
            1,
            theme::markdown_theme(),
            None,
            pi_tui::components::markdown::MarkdownOptions::default(),
        )));
        self.add_to_chat(Box::new(super::components::header::DynamicBorder::new()));
        self.tui.request_render();
    }

    /// `/changelog` (`handleChangelogCommand`, `interactive-mode.ts:6965-6984`):
    /// the changelog entries, newest first, bordered, as markdown.
    fn handle_changelog_command(&mut self) {
        let changelog_path = crate::changelog::get_changelog_path();
        let mut all_entries = crate::changelog::parse_changelog(&changelog_path);

        let changelog_markdown = if all_entries.is_empty() {
            "No changelog entries found.".to_string()
        } else {
            all_entries.reverse();
            all_entries
                .iter()
                .map(|entry| {
                    crate::changelog::normalize_changelog_links(
                        &entry.content,
                        &entry.version_string(),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        };

        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        self.add_to_chat(Box::new(super::components::header::DynamicBorder::new()));
        self.add_to_chat(Box::new(Text::new(
            theme::theme().fg(pi_tui::theme::ThemeColor::Accent, &bold_text("What's New")),
            1,
            0,
        )));
        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        self.add_to_chat(Box::new(pi_tui::components::markdown::Markdown::new(
            changelog_markdown,
            1,
            1,
            theme::markdown_theme(),
            None,
            pi_tui::components::markdown::MarkdownOptions::default(),
        )));
        self.add_to_chat(Box::new(super::components::header::DynamicBorder::new()));
        self.tui.request_render();
    }

    /// `/copy` (`handleCopyCommand`, `interactive-mode.ts:6828-6857`): copy
    /// the last agent message to the clipboard. The `flashConfirmation` /
    /// `preferSelection` options belong to the `app.message.copy` keybinding,
    /// whose alt-screen selection branch lands with that keybinding.
    fn handle_copy_command(&mut self) {
        let Some(text) = self.last_assistant_text() else {
            self.show_error("No agent messages to copy yet.");
            return;
        };
        match crate::clipboard::copy_to_clipboard(&text) {
            Ok(()) => self.show_status("Copied last agent message to clipboard"),
            Err(error) => self.show_error(&error),
        }
    }

    /// `getLastAssistantText` (`agent-session.ts:4528-4554`): the last
    /// assistant message's concatenated text blocks. A turn that did not end
    /// normally (aborted, failed, only tool calls) is skipped unless it still
    /// has text (#12119).
    fn last_assistant_text(&self) -> Option<String> {
        let mut text = String::new();
        for entry in self.session.get_entries().iter().rev() {
            let crate::session::records::SessionEntry::Message(entry) = entry else {
                continue;
            };
            let AgentMessage::Assistant(assistant) = &entry.message else {
                continue;
            };
            let has_text = assistant.content.iter().any(|block| {
                matches!(block, pi_ai::types::AssistantContent::Text(block) if !block.text.trim().is_empty())
            });
            if assistant.stop_reason != pi_ai::types::StopReason::Stop && !has_text {
                continue;
            }
            for block in &assistant.content {
                if let pi_ai::types::AssistantContent::Text(block) = block {
                    text.push_str(&block.text);
                }
            }
            break;
        }
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    // --------------------------------------------------------------
    // Theme controller (`theme/theme-controller.ts`)
    // --------------------------------------------------------------

    /// `applyFromSettings` (`theme-controller.ts:96-104`): resolve the
    /// setting against the terminal, sync mode-2031 notifications, apply,
    /// and issue a fresh color query.
    fn theme_apply_from_settings(&mut self, show_error: bool) -> Result<(), String> {
        let name = self.theme_resolve_name();
        self.theme_auto_sync =
            pi_tui::theme::parse_auto_theme_setting(self.theme_setting.as_deref()).is_some()
                || name == pi_tui::theme::SYSTEM_THEME_NAME;
        self.tui
            .set_color_scheme_notifications(self.theme_auto_sync);
        let result = self.theme_apply_name(&name, show_error);
        self.theme_issue_query();
        result
    }

    /// `resolveThemeName` (`theme-controller.ts:168-171`): the theme for the
    /// current setting and terminal appearance, defaulting to the system
    /// theme.
    fn theme_resolve_name(&self) -> String {
        theme::resolve_theme_setting(self.theme_setting.as_deref())
            .unwrap_or_else(|| pi_tui::theme::SYSTEM_THEME_NAME.to_string())
    }

    /// `applyThemeName` (`theme-controller.ts:173-183`): switch the global
    /// theme; a failure keeps the system theme. Returns the load result for
    /// callers that report it.
    fn theme_apply_name(&mut self, name: &str, show_error: bool) -> Result<(), String> {
        let result = theme::set_theme(name);
        self.theme_active_name = match &result {
            Ok(()) => name.to_string(),
            Err(_) => pi_tui::theme::SYSTEM_THEME_NAME.to_string(),
        };
        self.tui.invalidate();
        self.tui.request_render();
        if let Err(error) = &result
            && show_error
        {
            self.show_error(&format!(
                "Failed to load theme \"{name}\": {error}\nFell back to the system theme."
            ));
        }
        result
    }

    /// `requestTerminalColors` (`theme-controller.ts:28-46`): query the
    /// terminal's colors with the 100 ms budget; replies that arrive after
    /// the deadline ride the sink (`onLateReply`).
    fn theme_issue_query(&mut self) {
        self.theme_query_start = Some(Instant::now());
        let queue = Arc::clone(&self.theme_late_reports);
        let sink = Box::new(move |colors: TerminalColors| {
            if let Ok(mut pending) = queue.lock() {
                pending.push(colors);
            }
        });
        let _ = self.tui.query_terminal_colors(100, Some(sink));
    }

    /// `applyTerminalColors` (`theme-controller.ts:187-205`): record the
    /// report; re-render only when it changed.
    fn theme_apply_colors(&mut self, colors: TerminalColors) {
        if !theme::record_reported_colors(colors) {
            return;
        }
        self.theme_reapply_for_terminal();
        self.tui.invalidate();
        self.tui.request_render();
    }

    /// `reapplyForTerminal` (`theme-controller.ts:207-216`): regenerate the
    /// system theme or switch a pair's theme after the terminal's colors or
    /// appearance changed.
    fn theme_reapply_for_terminal(&mut self) {
        let name = self.theme_resolve_name();
        if name == pi_tui::theme::SYSTEM_THEME_NAME || name != self.theme_active_name {
            let _ = self.theme_apply_name(&name, false);
        }
    }

    /// `applyTerminalColorSchemeChange` (`theme-controller.ts:224-231`): the
    /// terminal reported a light/dark switch; its colors changed too, so
    /// re-apply the setting and query again.
    fn theme_apply_scheme(&mut self, scheme: TerminalColorScheme) {
        if !self.theme_auto_sync {
            return;
        }
        let appearance = match scheme {
            TerminalColorScheme::Dark => ThemeAppearance::Dark,
            TerminalColorScheme::Light => ThemeAppearance::Light,
        };
        let previous = theme::terminal_theme();
        theme::set_terminal_color_scheme(appearance);
        if theme::terminal_theme() != previous {
            self.theme_reapply_for_terminal();
        }
        self.theme_issue_query();
    }

    /// Drain terminal replies and theme queues. `pump` calls this so the
    /// controller runs on the UI thread like upstream's callbacks.
    fn pump_theme(&mut self) {
        while self.tui.poll_terminal_response() {}
        if let Some(start) = self.theme_query_start {
            let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.tui.service_terminal_queries(elapsed);
        }
        while let Some(colors) = self.tui.take_terminal_colors() {
            self.theme_apply_colors(colors);
        }
        // The drained Vec is bound first: a `for` over the `mem::take`
        // expression would keep the mutex guard's borrow of `self` alive
        // across the `&mut self` body.
        let late = std::mem::take(&mut *self.theme_late_reports.lock().expect("late colors"));
        for colors in late {
            self.theme_apply_colors(colors);
        }
        let schemes = std::mem::take(&mut *self.theme_scheme_reports.lock().expect("schemes"));
        for scheme in schemes {
            self.theme_apply_scheme(scheme);
        }
    }

    /// `/clear`: a fresh session, chat wiped (`handleClearCommand`).
    fn handle_clear(&mut self) {
        if self.agent_run.is_some() {
            self.show_error("Cannot start a new session while the agent is responding.");
            return;
        }
        // `emitBeforeSwitch("new")` (`agent-session-runtime.ts:133-146`):
        // a `cancel` vote aborts the switch. Teardown emits
        // `session_shutdown` with reason "new" (`teardownCurrent`,
        // `agent-session-runtime.ts:167-175`) before the replacement.
        if self.extension_cancelled(&serde_json::json!({
            "type": "session_before_switch",
            "reason": "new",
        })) {
            return;
        }
        self.emit_extension_shutdown("new", None);
        let cwd = self.session.get_cwd().to_path_buf();
        match SessionManager::create(&cwd, None, &NewSessionOptions::default()) {
            Ok(session) => {
                self.session = session;
                self.chat_mut().clear();
                self.pending_tools.clear();
                self.streaming_index = None;
                self.refresh_footer();
                self.tui.request_render();
            }
            Err(error) => self.show_error(&error.to_string()),
        }
    }

    /// `!command` (`handleBashCommand`): run synchronously, render as a shell
    /// row, persist as a bash-execution message. Upstream streams the output
    /// live through a PTY; the streaming pass lands with P7.
    fn run_bash(&mut self, command: &str, excluded: bool) {
        // Extensions intercept first (`handleBashCommand`,
        // `interactive-mode.ts:7172-7189`): a handler failure skips local
        // execution entirely (the runner already reported it); a decision
        // replaces the local run.
        if let Some(outcome) =
            self.extension_user_bash(command, excluded, &self.session.get_cwd().to_string_lossy())
        {
            match outcome {
                Err(message) => {
                    self.show_error(&format!("user_bash: {message}"));
                    return;
                }
                // No handler intercepted: the normal local run below.
                Ok(crate::extensions::UserBashOutcome::Local) => {}
                Ok(
                    crate::extensions::UserBashOutcome::Executed(result)
                    | crate::extensions::UserBashOutcome::Result(result),
                ) => {
                    self.record_bash_result(command, excluded, &result, Instant::now());
                    return;
                }
            }
        }
        let started = Instant::now();
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(self.session.get_cwd())
            .output();
        let (text, exit_code) = match output {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.is_empty() {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&stderr);
                }
                (text, Some(output.status.code().unwrap_or(-1) as i64))
            }
            Err(error) => (error.to_string(), None),
        };
        self.record_bash_output(command, excluded, text, exit_code, false, None, started);
    }

    /// The shared `!` tail — render the shell row and persist the
    /// bash-execution entry (`recordBashResult`): the output rides the
    /// entry, so `bashExecutionToText` shows it to the model exactly like
    /// upstream.
    #[allow(clippy::too_many_arguments)] // mirrors upstream's recordBashResult fields; split when extending
    fn record_bash_output(
        &mut self,
        command: &str,
        excluded: bool,
        text: String,
        exit_code: Option<i64>,
        truncated: bool,
        full_output_path: Option<String>,
        started: Instant,
    ) {
        let component = ToolExecutionComponent::new(
            "bash",
            &format!("bash-{}", self.chat_len()),
            serde_json::json!({ "command": command }),
            1,
            &self.session.get_cwd().to_string_lossy(),
            crate::tools::renderers::renderers_for("bash"),
        );
        let index = self.add_to_chat(Box::new(component));
        if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(index) {
            component.mark_execution_started();
            component.update_result(
                ToolResultView {
                    text: text.clone(),
                    is_error: exit_code.is_none_or(|code| code != 0),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                },
                false,
            );
        }
        let _ = self.session.append_message(AgentMessage::BashExecution(
            pi_agent::agent_message::BashExecutionMessage {
                command: command.to_string(),
                output: text,
                exit_code,
                cancelled: false,
                truncated,
                full_output_path,
                exclude_from_context: Some(excluded),
                timestamp: now_millis(),
            },
        ));
        self.tui.request_render();
    }

    /// The extension-decided path: the outcome is an upstream
    /// `BashResult` (`{output, exitCode, cancelled, truncated,
    /// fullOutputPath?}`) either verbatim or composed from the handler's
    /// operations.
    fn record_bash_result(
        &mut self,
        command: &str,
        excluded: bool,
        result: &serde_json::Value,
        started: Instant,
    ) {
        let text = result
            .get("output")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let exit_code = result.get("exitCode").and_then(serde_json::Value::as_i64);
        let truncated = result
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let full_output_path = result
            .get("fullOutputPath")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        self.record_bash_output(
            command,
            excluded,
            text,
            exit_code,
            truncated,
            full_output_path,
            started,
        );
    }

    /// The `user_bash` interception (`handleBashCommand`): `None` when
    /// headless — no runner, execute locally.
    fn extension_user_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
        cwd: &str,
    ) -> Option<Result<crate::extensions::UserBashOutcome, String>> {
        #[cfg(feature = "ext-js")]
        {
            let runner = self.extension_runner.as_ref()?;
            crate::extensions::user_bash(runner, command, exclude_from_context, cwd)
        }
        #[cfg(not(feature = "ext-js"))]
        {
            let _ = (command, exclude_from_context, cwd);
            None
        }
    }

    /// One `session_before_*` gate vote: `true` cancels the surrounding
    /// operation (`emitBeforeSwitch`/`session_before_compact`).
    fn extension_cancelled(&self, event: &serde_json::Value) -> bool {
        #[cfg(feature = "ext-js")]
        {
            let Some(runner) = &self.extension_runner else {
                return false;
            };
            match crate::extensions::emit_gate(runner, event) {
                Ok(cancelled) => cancelled,
                Err(error) => {
                    // Gate failures do not block the operation; the
                    // runner reported the handler error.
                    eprintln!("Warning: Extension gate error: {error}");
                    false
                }
            }
        }
        #[cfg(not(feature = "ext-js"))]
        {
            let _ = event;
            false
        }
    }

    /// `teardownCurrent`'s `session_shutdown` emission before a session
    /// replacement (`agent-session-runtime.ts:167-175`).
    fn emit_extension_shutdown(&self, reason: &str, target_session_file: Option<&str>) {
        #[cfg(feature = "ext-js")]
        {
            let Some(runner) = &self.extension_runner else {
                return;
            };
            let mut event = serde_json::json!({
                "type": "session_shutdown",
                "reason": reason,
            });
            if let Some(target) = target_session_file {
                event["targetSessionFile"] = serde_json::json!(target);
            }
            if let Err(error) = crate::extensions::emit_gate(runner, &event) {
                eprintln!("Warning: Extension error: session_shutdown: {error}");
            }
        }
        #[cfg(not(feature = "ext-js"))]
        {
            let _ = (reason, target_session_file);
        }
    }

    /// The `transformContext` hook (`sdk.ts:430-434`) for the agent-loop
    /// config; `None` without the engine or without context handlers.
    fn transform_context(&self) -> Option<Arc<dyn pi_agent::TransformContext>> {
        #[cfg(feature = "ext-js")]
        {
            let runner = self.extension_runner.as_ref()?;
            crate::extensions::context_transform(Arc::clone(runner))
        }
        #[cfg(not(feature = "ext-js"))]
        {
            None
        }
    }

    // --------------------------------------------------------------
    // Agent run control
    // --------------------------------------------------------------

    /// Start one agent run on a worker thread (`session.prompt`).
    fn start_agent_run(&mut self, text: &str) {
        self.editor_mut().editor.add_to_history(text);

        // Session context from the persisted entries (before the new prompt).
        let leaf_id = self.session.get_leaf_id().map(str::to_string);
        let session_context = crate::session::store::build_session_context_from(
            &self.session.get_entries(),
            leaf_id.as_deref(),
        );
        let mut context_messages = session_context.messages;
        if context_messages.is_empty() {
            if let Some(system) = pi_agent::transcript::create_initial_system_message(
                Some(&self.system_prompt),
                &self
                    .executor
                    .tools()
                    .iter()
                    .map(tool_declaration)
                    .collect::<Vec<_>>(),
            ) {
                context_messages.push(AgentMessage::System(system));
            }
        }

        let user_message = pi_agent::transcript::user_message(text, now_millis());
        let _ = self.session.append_message(user_message.clone());
        let AgentMessage::User(user) = &user_message else {
            unreachable!("user_message builds a user message");
        };
        self.active_prompt = Some((user.timestamp, text.to_string()));

        let context = AgentContext {
            messages: context_messages,
            tools: self.executor.tools(),
        };
        // Under a virtual selection every request routes first and the
        // stream dispatches per routed model.
        #[cfg(feature = "ext-js")]
        let routing = crate::virtual_models::TurnRouting::install(
            &self.model,
            self.extension_runner.as_ref(),
            self.api_key.clone(),
            &self.session,
        );
        let config = AgentLoopConfig {
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            live_settings: None,
            session_id: None,
            api_key: self.api_key.clone(),
            access_token: self.access_token.clone(),
            tool_execution: ToolExecutionMode::Parallel,
            hooks: pi_agent::types::ToolCallHooks {
                before: None,
                after: None,
            },
            finish_turn: None,
            #[cfg(feature = "ext-js")]
            prepare_request: routing.as_ref().map(|routing| {
                Arc::clone(&routing.hook) as Arc<dyn pi_agent::agent_loop::PrepareRequest>
            }),
            #[cfg(not(feature = "ext-js"))]
            prepare_request: None,
            prepare_next_turn: None,
            transform_context: self.transform_context(),
            convert_to_llm: Arc::new(DefaultConvertToLlm),
        };

        let (tx, rx) = std::sync::mpsc::channel::<LoopEvent>();
        let signal = Arc::new(AbortSignal::new());
        let executor = Arc::clone(&self.executor);
        #[cfg(feature = "ext-js")]
        let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = match &routing {
            Some(routing) => routing.stream_fn(),
            None => Arc::clone(&self.stream_fn),
        };
        #[cfg(not(feature = "ext-js"))]
        let loop_stream_fn: Arc<dyn pi_agent::types::StreamFn> = Arc::clone(&self.stream_fn);
        let stream_fn = loop_stream_fn;
        let run_signal = Arc::clone(&signal);
        let shared_queues = SharedQueues(Arc::clone(&self.queues));
        let mut shared_queues = shared_queues;
        let handle = std::thread::spawn(move || {
            let mut sink = ChannelSink { tx: tx.clone() };
            let _messages = crate::runtime::block_on(pi_agent::run_agent_loop(
                vec![user_message],
                context,
                executor.as_ref(),
                &config,
                &mut shared_queues,
                &mut sink,
                &run_signal,
                stream_fn.as_ref(),
            ));
            let _ = tx.send(LoopEvent::Done);
        });
        self.agent_run = Some(AgentRun {
            abort: signal,
            events: rx,
            handle: Some(handle),
            #[cfg(feature = "ext-js")]
            routing: routing.map(Arc::new),
        });
        self.set_working_status(Some(StatusIndicator::working("Working...")));
        self.tui.request_render();
    }

    fn set_working_status(&mut self, indicator: Option<StatusIndicator>) {
        // The editor's border decorator reads the same slot every frame.
        *self.status_handle.lock().expect("status") = indicator;
    }

    /// `showStatus` (`interactive-mode.ts:3964`): a muted line in the chat.
    fn show_status(&mut self, message: &str) {
        self.add_to_chat(Box::new(Text::new(
            theme::theme().fg(pi_tui::theme::ThemeColor::Muted, message),
            1,
            0,
        )));
        self.tui.request_render();
    }

    /// `showError`: a red line in the chat.
    fn show_error(&mut self, message: &str) {
        self.add_to_chat(Box::new(Text::new(
            theme::theme().fg(pi_tui::theme::ThemeColor::Error, message),
            1,
            0,
        )));
        self.tui.request_render();
    }

    // --------------------------------------------------------------
    // Main loop (`run`, `interactive-mode.ts:1187`)
    // --------------------------------------------------------------

    fn run(&mut self) -> Result<(), String> {
        use std::io::Read as _;
        self.tui
            .start()
            .map_err(|error| format!("failed to start the terminal: {error}"))?;
        self.tui.request_render();
        // Raw stdin on its own thread: in raw mode the bytes are the
        // terminal's, and pi's stdin buffer needs them whole. crossterm's
        // event parser would eat terminal replies (OSC colors, mode 2031)
        // and deliver escape-sequence garbage instead.
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let spawned = std::thread::Builder::new()
            .name("stdin".to_string())
            .spawn(move || {
                let mut stdin = std::io::stdin().lock();
                let mut chunk = [0u8; 8192];
                loop {
                    match stdin.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            if tx.send(chunk[..count].to_vec()).is_err() {
                                break;
                            }
                        }
                    }
                }
            })
            .is_ok();
        let stdin_rx = spawned.then_some(rx);
        // `init` mounts the fullscreen viewport on the alt-screen renderer
        // before the first frame (`interactive-mode.ts:713-731`); the port's
        // single `Tui` enters it here instead.
        if self.tui_mode == TuiMode::Fullscreen
            && let Err(error) = self.enter_fullscreen()
        {
            let _ = self.shutdown();
            return Err(error);
        }
        // A resumed session replays its transcript (`renderInitialMessages`).
        self.render_initial_messages();
        loop {
            self.pump();
            if self.exiting {
                break;
            }
            self.await_stdin(stdin_rx.as_ref());
            self.tui.render_now(false);
        }

        self.shutdown()
    }

    /// Wait for stdin bytes (25 ms budget), decode and dispatch them, and
    /// flush the stdin buffer once a pending sequence outlives its budget
    /// (`stdin-buffer.ts:380-390` — the host owns the clock).
    fn await_stdin(&mut self, rx: Option<&std::sync::mpsc::Receiver<Vec<u8>>>) {
        let Some(rx) = rx else {
            std::thread::sleep(Duration::from_millis(25));
            return;
        };
        let received = rx.recv_timeout(Duration::from_millis(25));
        match received {
            Ok(bytes) => {
                self.stdin_last_input = Some(Instant::now());
                self.feed_stdin_bytes(&bytes);
                // A burst arrives as several chunks; drain what is queued so
                // it is one frame.
                while let Ok(more) = rx.try_recv() {
                    self.stdin_last_input = Some(Instant::now());
                    self.feed_stdin_bytes(&more);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if let Some(timeout_ms) = self.stdin_buffer.pending_timeout_ms()
            && self
                .stdin_last_input
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(timeout_ms))
        {
            let mut events = Vec::new();
            self.stdin_buffer.flush(&mut events);
            self.dispatch_stdin_events(events);
        }
    }

    /// Decode raw bytes as UTF-8 (keeping an incomplete tail for the next
    /// chunk) and run the decoded text through the stdin buffer.
    fn feed_stdin_bytes(&mut self, bytes: &[u8]) {
        self.stdin_pending.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.stdin_pending) {
                Ok(text) => {
                    let mut events = Vec::new();
                    self.stdin_buffer.process(text, &mut events);
                    self.dispatch_stdin_events(events);
                    self.stdin_pending.clear();
                    return;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        // `error` does not borrow, but the slice does: copy
                        // the valid prefix out before draining.
                        let text =
                            String::from_utf8_lossy(&self.stdin_pending[..valid]).to_string();
                        let mut events = Vec::new();
                        self.stdin_buffer.process(&text, &mut events);
                        self.dispatch_stdin_events(events);
                        self.stdin_pending.drain(..valid);
                    }
                    match error.error_len() {
                        Some(skip) => {
                            self.stdin_pending.drain(..skip);
                        }
                        None => return,
                    }
                }
            }
        }
    }

    /// Route decoded events: sequences straight to the TUI, pastes re-wrapped
    /// with their markers for the editor (`terminal.ts:252-278`).
    fn dispatch_stdin_events(&mut self, events: Vec<StdinEvent>) {
        for event in events {
            match event {
                StdinEvent::Data(sequence) => {
                    self.tui.handle_input(&sequence);
                }
                StdinEvent::Paste(content) => {
                    let wrapped = format!("\x1b[200~{content}\x1b[201~");
                    self.tui.handle_input(&wrapped);
                }
            }
        }
    }

    /// The model-selector drain (`showSelector`'s onSelect/onCancel
    /// callbacks). Takes the queued events out first, so the mutex guard
    /// does not pin `self` across the handlers.
    fn pump_model_selector(&mut self) {
        if self.model_selector_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .model_selector_events
                .as_ref()
                .expect("selector")
                .lock()
                .expect("selector events"),
        );
        for event in events {
            match event {
                ModelSelectorEvent::Selected(model) => {
                    self.close_model_selector();
                    if let Err(error) = self.apply_model(model) {
                        self.show_error(&error);
                    } else {
                        self.show_status(&format!("Model: {}", self.model.id));
                    }
                }
                ModelSelectorEvent::Cancelled => self.close_model_selector(),
            }
        }
    }

    /// The `/trust` selector's drain (`showTrustSelector`'s
    /// onSelect/onCancel): the decision persists to trust.json and the
    /// status tells the user a restart applies it.
    fn pump_trust_selector(&mut self) {
        if self.trust_selector_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .trust_selector_events
                .as_ref()
                .expect("trust selector")
                .lock()
                .expect("trust selector events"),
        );
        for event in events {
            match event {
                TrustSelectorEvent::Select { trusted, updates } => {
                    self.close_trust_selector();
                    let store = ProjectTrustStore::new(&self.agent_dir);
                    match store.set_many(&updates) {
                        Ok(()) => {
                            let label = if trusted { "trusted" } else { "untrusted" };
                            self.show_status(&format!(
                                "Saved trust decision: {label}. Restart {} for this to take effect.",
                                crate::config::APP_NAME
                            ));
                        }
                        Err(error) => self.show_error(&error),
                    }
                }
                TrustSelectorEvent::Cancel => {
                    self.close_trust_selector();
                    self.tui.request_render();
                }
            }
        }
    }

    /// The `/settings` dialog's drain (`showSettingsSelector`'s callbacks).
    fn pump_settings_selector(&mut self) {
        if self.settings_selector_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .settings_selector_events
                .as_ref()
                .expect("settings selector")
                .lock()
                .expect("settings selector events"),
        );
        for event in events {
            self.handle_settings_event(event);
        }
    }

    /// Mount the `/resume` dialog in the editor's tree slot
    /// (`showSessionSelector`, `interactive-mode.ts:4900-4926`).
    fn open_session_selector(&mut self) {
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = SessionSelectorComponent::new(
            self.session.get_cwd(),
            Some(self.session.get_session_dir()),
            self.session.uses_default_session_dir(),
            self.session.session_file(),
            Arc::clone(&events),
        );
        self.session_selector_events = Some(events);
        self.suspend_editor_with(Box::new(selector));
    }

    /// Put the editor back after the resume dialog closes.
    fn close_session_selector(&mut self) {
        if self.suspended_editor.is_none() {
            return;
        }
        self.session_selector_events = None;
        self.restore_editor();
    }

    // --------------------------------------------------------------
    // TUI mode (`switchTuiMode`, `interactive-mode.ts:641-700`)
    // --------------------------------------------------------------

    /// The alt-screen options `createInteractiveTui` builds for fullscreen
    /// (`tui-renderer.js:9-29`): the theme-styled search matches, the themed
    /// jump-to-end label, and the fullscreen settings. `open_url` /
    /// `on_right_click_paste` stay unset — the port has no browser opener,
    /// and right-click paste is a Windows-terminal feature upstream.
    fn fullscreen_options(&self) -> AltScreenOptions {
        let theme = theme::theme();
        let search_match_style = {
            let theme = std::sync::Arc::clone(&theme);
            std::sync::Arc::new(move |text: &str| {
                let base = theme.bg(
                    pi_tui::theme::ThemeBg::SearchMatchBg,
                    &theme.fg(pi_tui::theme::ThemeColor::SearchMatchText, text),
                );
                style_text_with_ansi(
                    &base,
                    None,
                    None,
                    TextAttributes {
                        underline: true,
                        ..TextAttributes::default()
                    },
                )
            })
        };
        let search_current_match_style = {
            let theme = std::sync::Arc::clone(&theme);
            std::sync::Arc::new(move |text: &str| {
                let base = theme.bg(
                    pi_tui::theme::ThemeBg::SearchMatchBg,
                    &theme.fg(pi_tui::theme::ThemeColor::SearchMatchText, text),
                );
                style_text_with_ansi(
                    &base,
                    None,
                    None,
                    TextAttributes {
                        bold: true,
                        inverse: true,
                        ..TextAttributes::default()
                    },
                )
            })
        };
        let search_navigation_button_style: pi_tui::alt_screen_search::SearchButtonStyle = {
            std::sync::Arc::new(|text: &str, hovered: bool| {
                if !hovered {
                    return text.to_string();
                }
                style_text_with_ansi(
                    text,
                    None,
                    None,
                    TextAttributes {
                        underline: true,
                        ..TextAttributes::default()
                    },
                )
            })
        };
        let shortcut = key_display_text(pi_tui::keybindings::action::ALT_SCREEN_BOTTOM);
        let scroll_to_end_indicator = {
            let theme = std::sync::Arc::clone(&theme);
            Box::new(move || {
                let label = if shortcut.is_empty() {
                    " ↓ Jump to latest message ".to_string()
                } else {
                    format!(" ↓ Jump to latest message · {shortcut} ")
                };
                theme.bg(
                    pi_tui::theme::ThemeBg::SelectedBg,
                    &theme.fg(pi_tui::theme::ThemeColor::Text, &label),
                )
            }) as Box<dyn Fn() -> String + Send>
        };
        AltScreenOptions {
            mouse: true,
            wheel_scroll_lines: Some(self.settings.get_fullscreen_wheel_scroll_lines()),
            scroll_to_end_indicator: Some(scroll_to_end_indicator),
            open_url: None,
            on_right_click_paste: None,
            copy_on_select: self.settings.get_fullscreen_copy_on_select(),
            search_match_style,
            search_current_match_style,
            search_navigation_button_style,
        }
    }

    /// Mount the fullscreen viewport: the flat hosts move into the chat
    /// viewport on the alt screen (`switchTuiMode("fullscreen")`'s
    /// `mountInteractiveTui` + `setLayoutRoot`, `interactive-mode.ts:672-680`).
    fn enter_fullscreen(&mut self) -> Result<(), String> {
        let hosts = self.tui.take_root_children();
        let scrollbar = self.settings.get_fullscreen_scrollbar().into();
        let options = self.fullscreen_options();
        if let Err(error) = self.tui.enter_alt_screen(options) {
            // The terminal refused the alt screen; put the flat tree back.
            for host in hosts {
                self.tui.add_child(host);
            }
            return Err(format!("failed to enter fullscreen: {error}"));
        }
        self.viewport = Some(chat_viewport::mount(&mut self.tui, hosts, scrollbar));
        self.tui.set_focus(Some(FocusTarget::Id(self.editor_id)));
        Ok(())
    }

    /// Unmount the fullscreen viewport and restore the flat tree. The
    /// counterpart of upstream rebuilding the regular renderer and remounting
    /// the same components (`interactive-mode.ts:653-680`); the alt screen
    /// leaves with `preserveScreen` and the next regular frame repaints.
    fn exit_fullscreen(&mut self) {
        let Some(viewport) = self.viewport.take() else {
            return;
        };
        let hosts = chat_viewport::disassemble(&viewport, &mut self.tui);
        let leftover = self.tui.exit_alt_screen(TuiStopOptions {
            preserve_screen: true,
        });
        debug_assert!(
            leftover.is_empty(),
            "the viewport owns every root child while fullscreen"
        );
        for host in hosts {
            self.tui.add_child(host);
        }
        self.tui.set_focus(Some(FocusTarget::Id(self.editor_id)));
    }

    /// Swap the renderer under the component tree
    /// (`switchTuiMode`, `interactive-mode.ts:641-700`). Returns false while
    /// a selector occupies the editor slot — upstream refuses the switch
    /// while overlay entries are open (`:645`) — and true when the mode
    /// already matched (`:643`).
    fn switch_tui_mode(&mut self, mode: TuiMode) -> bool {
        if mode == self.tui_mode {
            return true;
        }
        if self.suspended_editor.is_some() {
            return false;
        }
        match mode {
            TuiMode::Fullscreen => {
                if self.enter_fullscreen().is_err() {
                    return false;
                }
            }
            TuiMode::Regular => self.exit_fullscreen(),
        }
        self.tui_mode = mode;
        self.tui.render_now(true);
        true
    }

    /// Swap `component` into the editor's slot — the flat root index in the
    /// regular renderer, the dock slot inside the viewport in fullscreen —
    /// and return the previous occupant (`showSelector`'s editor-container
    /// swap, `interactive-mode.ts:5085-5090`).
    fn replace_editor_slot(&mut self, component: Box<dyn Component>) -> Option<Box<dyn Component>> {
        let dock_id = self.viewport.as_ref().map(ChatViewport::dock_id);
        if let Some(dock_id) = dock_id {
            return self
                .tui
                .component_by_id_deep_mut(dock_id)?
                .as_any_mut()?
                .downcast_mut::<VStack>()?
                .stack()
                .replace_child(HostSlot::Editor.dock_index(), component)
                .into();
        }
        Some(self.tui.replace_child(HostSlot::Editor.index(), component))
    }

    /// `showSelector`'s mount (`interactive-mode.ts:5085-5093`): swap
    /// `component` into the editor's slot, park the editor, and focus the
    /// new occupant by identity.
    fn suspend_editor_with(&mut self, component: Box<dyn Component>) {
        let focused = component_id(component.as_ref());
        let editor = self
            .replace_editor_slot(component)
            .expect("the editor slot holds a component");
        self.suspended_editor = Some(editor);
        self.tui.set_focus(Some(FocusTarget::Id(focused)));
        self.tui.request_render();
    }

    /// `showSelector`'s `done` (`interactive-mode.ts:5096-5103`): put the
    /// editor back and focus it.
    fn restore_editor(&mut self) {
        if let Some(editor) = self.suspended_editor.take() {
            drop(self.replace_editor_slot(editor));
        }
        self.tui.set_focus(Some(FocusTarget::Id(self.editor_id)));
        self.tui.request_render();
    }

    /// `stopInteractiveTui`'s overlay drain (`interactive-mode.ts:634-636`):
    /// put the editor back and drop the selector queues, so the transcript
    /// exit leaves a plain transcript behind.
    fn drain_overlays_for_exit(&mut self) {
        if let Some(editor) = self.suspended_editor.take() {
            drop(self.replace_editor_slot(editor));
        }
        self.model_selector_events = None;
        self.trust_selector_events = None;
        self.settings_selector_events = None;
        self.session_selector_events = None;
        self.auth_selector_events = None;
        self.login_prompt_events = None;
        self.login_prompt_provider = None;
        self.tui.set_focus(Some(FocusTarget::Id(self.editor_id)));
    }

    /// `handleResumeSession` (`interactive-mode.ts:4928-4974`): swap the
    /// session store and replay the target's transcript. Deviation: upstream
    /// rebuilds the whole runtime for the target's cwd (extensions, tools,
    /// trust); the port keeps the startup runtime and only swaps the
    /// session, so a foreign-cwd session keeps this cwd's tool bindings.
    fn switch_session(&mut self, session_path: &std::path::Path) {
        if self.agent_run.is_some() {
            self.abort_active_run();
        }
        // `emitBeforeSwitch("resume", targetSessionFile)`
        // (`agent-session-runtime.ts:148-166`), then `teardownCurrent`
        // emits `session_shutdown` with the destination
        // (`agent-session-runtime.ts:167-175`) before the replacement.
        let target = session_path.to_string_lossy().into_owned();
        if self.extension_cancelled(&serde_json::json!({
            "type": "session_before_switch",
            "reason": "resume",
            "targetSessionFile": target,
        })) {
            return;
        }
        self.emit_extension_shutdown("resume", Some(&target));
        let session_dir = Some(self.session.get_session_dir().to_path_buf());
        match SessionManager::open(session_path, session_dir.as_deref(), None) {
            Ok(session) => {
                self.session = session;
                self.active_prompt = None;
                self.queued_deliveries.clear();
                self.pending_tools.clear();
                self.streaming_index = None;
                self.compaction_run = None;
                self.render_initial_messages();
                self.refresh_footer();
                self.show_status("Resumed session");
            }
            Err(error) => {
                self.show_status(&format!("Failed to resume session: {error}"));
            }
        }
    }

    /// The `/resume` dialog's events (`showSessionSelector`'s callbacks,
    /// `interactive-mode.ts:4910-4917`).
    fn handle_session_event(&mut self, event: SessionSelectorEvent) {
        match event {
            SessionSelectorEvent::Selected(path) => {
                self.close_session_selector();
                self.switch_session(&path);
            }
            SessionSelectorEvent::Cancel => self.close_session_selector(),
        }
    }

    fn pump_session_selector(&mut self) {
        if self.session_selector_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .session_selector_events
                .as_ref()
                .expect("session selector")
                .lock()
                .expect("session selector events"),
        );
        for event in events {
            self.handle_session_event(event);
        }
    }

    // --------------------------------------------------------------
    // /login and /logout (`handleLoginCommand`, `interactive-mode.ts:6053-6075`,
    // `showOAuthSelector` `:6209-6251`, the auth-type menu `:6087-6155`,
    // `startProviderLogin` `:6077-6085`, the login dialogs `:6367-6616`,
    // and `completeProviderAuthentication`'s status lines `:6303-6359`)
    // --------------------------------------------------------------

    /// `formatAuthSelectorProviderStatus` with the undefined-status fallback
    /// (`oauth-selector.ts:31-33`): " • not configured" rides the muted color.
    fn auth_selector_status_suffix(provider: &AuthSelectorProvider) -> String {
        match &provider.status {
            Some(status) => oauth::format_auth_selector_provider_status(provider, status),
            None => theme::theme().fg(pi_tui::theme::ThemeColor::Muted, " • not configured"),
        }
    }

    /// `/login [provider]` (`handleLoginCommand`): one match starts its
    /// login, matches sharing one id open the auth-type selector, anything
    /// else opens the provider selector seeded with the reference.
    fn handle_login_command(&mut self, provider_ref: Option<&str>) {
        let Some(provider_ref) = provider_ref else {
            self.show_login_auth_type_selector(None);
            return;
        };
        let options = oauth::login_provider_options(&self.auth_path);
        let matches = oauth::find_matching_providers(&options, provider_ref);
        if let [option] = matches.as_slice() {
            self.start_provider_login(option, None);
            return;
        }
        if matches.len() > 1 {
            let same_id = matches.iter().all(|provider| provider.id == matches[0].id);
            if same_id {
                let provider_options: Vec<AuthSelectorProvider> =
                    matches.into_iter().cloned().collect();
                self.show_login_auth_type_selector(Some(provider_options.as_slice()));
                return;
            }
        }
        self.show_login_provider_selector(None, Some(provider_ref));
    }

    /// `showOAuthSelector("logout")` (`interactive-mode.ts:6209-6251`): the
    /// stored credentials, or the verbatim empty state. `"login"` routes to
    /// the auth-type menu (`:6210-6213`).
    fn show_oauth_selector(&mut self, mode: &str) {
        if mode == "login" {
            self.show_login_auth_type_selector(None);
            return;
        }
        let options = match oauth::logout_provider_options(&self.auth_path) {
            Ok(options) => options,
            Err(error) => {
                self.show_error(&format!("Could not read stored credentials: {error}"));
                return;
            }
        };
        if options.is_empty() {
            self.show_status(
                "No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.",
            );
            return;
        }
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = AuthSelectorComponent::logout(options, Arc::clone(&events));
        self.auth_selector_events = Some(events);
        self.auth_selector_mode = Some("logout");
        self.suspend_editor_with(Box::new(selector));
    }

    /// `showLoginAuthTypeSelector` (`interactive-mode.ts:6087-6155`): the
    /// subscription/API-key options for one provider, or the top-level menu
    /// with the Radius option last. `provider_options` carries the one
    /// provider's entries when `/login <ref>` matched several auth types.
    fn show_login_auth_type_selector(&mut self, provider_options: Option<&[AuthSelectorProvider]>) {
        let all_options = oauth::login_provider_options(&self.auth_path);
        // The top-level selector offers Radius directly, as its last option
        // (`interactive-mode.ts:6090-6093`).
        let radius_option = if provider_options.is_some() {
            None
        } else {
            all_options
                .iter()
                .find(|provider| {
                    provider.id == oauth::RADIUS_PROVIDER_ID && provider.auth_type == "oauth"
                })
                .cloned()
        };
        let radius_label = radius_option.as_ref().map(|provider| {
            format!(
                "Sign in with {}{}",
                provider.name,
                Self::auth_selector_status_suffix(provider)
            )
        });
        // `oauthLoginLabel` (`:6094-6098`): the OAuth method's selector label.
        let oauth_login_label = provider_options
            .as_ref()
            .and_then(|options| {
                options
                    .iter()
                    .find(|provider| provider.auth_type == "oauth")
            })
            .and_then(|provider| provider.login_label);
        let subscription_label = oauth_login_label
            .unwrap_or("Sign in with an account")
            .to_string();
        let api_key_label = "Sign in with an API key";
        let available = |auth_type: &str| {
            provider_options.is_none_or(|options| {
                options
                    .iter()
                    .any(|provider| provider.auth_type == auth_type)
            })
        };
        let mut options: Vec<String> = Vec::new();
        if available("oauth") {
            options.push(subscription_label.clone());
        }
        if available("api_key") {
            options.push(api_key_label.to_string());
        }
        if let Some(radius_label) = &radius_label {
            options.push(radius_label.clone());
        }
        if options.is_empty() {
            self.show_status("No login methods available.");
            return;
        }
        if provider_options.is_some() && options.len() == 1 {
            // The single auth type starts directly, without a back target
            // (`interactive-mode.ts:6130-6138`).
            if let Some(provider) = provider_options.and_then(|options| options.first()) {
                self.start_provider_login(provider, None);
            }
            return;
        }
        let title = provider_options
            .and_then(|options| options.first())
            .map_or_else(
                || "Select authentication method:".to_string(),
                |provider| format!("Select authentication method for {}:", provider.name),
            );
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = AuthSelectorComponent::options(&title, options, Arc::clone(&events));
        self.auth_selector_events = Some(events);
        self.auth_selector_mode = Some("login");
        self.pending_auth_type_options = provider_options.map(<[AuthSelectorProvider]>::to_vec);
        self.suspend_editor_with(Box::new(selector));
    }

    /// `showLoginProviderSelector` (`interactive-mode.ts:6156-6187`): the
    /// provider rows for one auth type, or all of them.
    fn show_login_provider_selector(
        &mut self,
        auth_type: Option<&'static str>,
        initial_search: Option<&str>,
    ) {
        let all_options = oauth::login_provider_options(&self.auth_path);
        let provider_options: Vec<AuthSelectorProvider> = all_options
            .into_iter()
            .filter(|provider| auth_type.is_none_or(|filter| provider.auth_type == filter))
            .collect();
        if provider_options.is_empty() {
            let message = match auth_type {
                Some("oauth") => "No account providers available.",
                Some("api_key") => "No API key providers available.",
                _ => "No login providers available.",
            };
            self.show_status(message);
            return;
        }
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector =
            AuthSelectorComponent::login(provider_options, initial_search, Arc::clone(&events));
        self.auth_selector_events = Some(events);
        self.auth_selector_mode = Some("login");
        self.pending_auth_type = auth_type;
        self.suspend_editor_with(Box::new(selector));
    }

    /// `startProviderLogin` (`interactive-mode.ts:6077-6085`): the oauth
    /// flow, the API-key prompt, or the ambient notice.
    fn start_provider_login(
        &mut self,
        provider: &AuthSelectorProvider,
        back: Option<LoginBackStep>,
    ) {
        if provider.auth_type == "oauth" {
            // The per-provider OAuth flows (PKCE/device-code/callback
            // servers, `packages/ai/src/auth/oauth/*.ts`) are not ported
            // yet; the upstream error shape reports the honest reason.
            self.show_error(&format!(
                "Failed to login to {}: OAuth login flows are not implemented in pi-rs yet",
                provider.name
            ));
        } else {
            // Custom select-based API-key flows (`amazon-bedrock.ts:13`,
            // `google-vertex.ts:15`) and the ambient-only auths resolve
            // outside the app; the standard `envApiKeyAuth` login and
            // the composed default (`provider-composer.ts:438-441`)
            // prompt for the key.
            if crate::oauth::AMBIENT_LOGIN_PROVIDERS.contains(&provider.id.as_str()) {
                self.show_status(&format!(
                    "{} is configured outside {}.",
                    provider.method_name.as_deref().unwrap_or("Authentication"),
                    crate::config::APP_NAME
                ));
                return;
            }
            let auth_name = provider
                .method_name
                .as_deref()
                .unwrap_or("API key")
                .to_string();
            self.open_login_prompt(&provider.id, &provider.name, &auth_name, back);
        }
    }

    /// `showApiKeyLoginDialog` + `loginProvider`'s prompt wiring
    /// (`interactive-mode.ts:6397-6447`, `showAuthPrompt` `:6483-6505`):
    /// mount the entry dialog over the editor slot.
    fn open_login_prompt(
        &mut self,
        provider_id: &str,
        provider_name: &str,
        auth_name: &str,
        back: Option<LoginBackStep>,
    ) {
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let prompt =
            LoginPromptComponent::new(&format!("Enter {auth_name}"), None, Arc::clone(&events));
        self.login_prompt_events = Some(events);
        self.login_prompt_provider = Some((provider_id.to_string(), provider_name.to_string()));
        self.login_back = back;
        self.suspend_editor_with(Box::new(prompt));
    }

    /// The login prompt's drain (`showPrompt`'s resolver/rejecter).
    fn pump_login_prompt(&mut self) {
        if self.login_prompt_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .login_prompt_events
                .as_ref()
                .expect("login prompt")
                .lock()
                .expect("login prompt events"),
        );
        for event in events {
            match event {
                LoginPromptEvent::Submit(value) => {
                    let Some((provider_id, provider_name)) = self.login_prompt_provider.take()
                    else {
                        continue;
                    };
                    self.restore_editor();
                    self.login_prompt_events = None;
                    self.login_back = None;
                    // Persist the credential (`AuthStorage.modify`, the
                    // `api_key` shape; the secret prompt resolves upstream in
                    // `envApiKeyAuth.login`, `helpers.ts:10-15`).
                    let storage = crate::auth::AuthStorage::create(&self.auth_path);
                    let result = storage.modify(&provider_id, |_current| {
                        Ok(Some(crate::auth::Credential::ApiKey {
                            key: Some(value.clone()),
                            env: None,
                            unknown: serde_json::Map::new(),
                        }))
                    });
                    match result {
                        Ok(_) => self.show_status(&format!(
                            "Saved API key for {provider_name}. Credentials saved to {}",
                            self.auth_path.display()
                        )),
                        Err(error) => self.show_error(&format!(
                            "Failed to save API key for {provider_name}: {error}"
                        )),
                    }
                }
                LoginPromptEvent::Cancel => {
                    // Upstream rejects with `Login cancelled`, which reopens
                    // the selector the login started from (`:6441-6443`).
                    self.restore_editor();
                    self.login_prompt_events = None;
                    self.login_prompt_provider = None;
                    let back = self.login_back.take();
                    self.run_login_back(back);
                }
            }
        }
    }

    /// The `onBack` chain (`showApiKeyLoginDialog`/`showLoginDialog`'s
    /// `errorMsg === "Login cancelled"` branch).
    fn run_login_back(&mut self, back: Option<LoginBackStep>) {
        match back {
            Some(LoginBackStep::AuthType(options)) => {
                self.show_login_auth_type_selector(options.as_deref());
            }
            Some(LoginBackStep::Provider(auth_type)) => {
                self.show_login_provider_selector(auth_type, None);
            }
            None => self.tui.request_render(),
        }
    }

    /// The mounted auth selector's drain (`showSelector`'s onSelect/onCancel
    /// callbacks): login rows start their flow, logout rows delete the
    /// stored credential, plain options drive the auth-type menu.
    fn pump_auth_selector(&mut self) {
        if self.auth_selector_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .auth_selector_events
                .as_ref()
                .expect("auth selector")
                .lock()
                .expect("auth selector events"),
        );
        for event in events {
            match event {
                AuthSelectorEvent::Select {
                    provider_id,
                    auth_type,
                } => {
                    // The pending-selector state feeds the `onBack` chains;
                    // it is captured before `close_auth_selector` clears it.
                    let mode = self.auth_selector_mode;
                    let pending_type = self.pending_auth_type;
                    let pending_options = self.pending_auth_type_options.clone();
                    self.close_auth_selector();
                    match mode {
                        Some("logout") => self.logout_provider(&provider_id, auth_type),
                        _ => self.start_selected_login(
                            &provider_id,
                            auth_type,
                            pending_type,
                            pending_options,
                        ),
                    }
                }
                AuthSelectorEvent::SelectOption(option) => {
                    let pending_options = self.pending_auth_type_options.clone();
                    self.close_auth_selector();
                    self.handle_auth_type_option(&option, pending_options);
                }
                AuthSelectorEvent::Cancel => {
                    self.close_auth_selector();
                    self.tui.request_render();
                }
            }
        }
    }

    /// Put the editor back after the auth selector closes.
    fn close_auth_selector(&mut self) {
        if self.auth_selector_events.is_some() {
            self.auth_selector_events = None;
            self.auth_selector_mode = None;
            self.pending_auth_type = None;
            self.pending_auth_type_options = None;
            self.restore_editor();
        }
    }

    /// A login row confirmed from the provider selector
    /// (`OAuthSelectorComponent`'s login `onSelect`,
    /// `interactive-mode.ts:6165-6184`): the pending state carries the
    /// `onBack` target.
    fn start_selected_login(
        &mut self,
        provider_id: &str,
        auth_type: &'static str,
        pending_type: Option<&'static str>,
        pending_options: Option<Vec<AuthSelectorProvider>>,
    ) {
        let options = oauth::login_provider_options(&self.auth_path);
        let Some(provider) = options
            .into_iter()
            .find(|provider| provider.id == provider_id && provider.auth_type == auth_type)
        else {
            return;
        };
        let back = match pending_type {
            Some(auth_type) => Some(LoginBackStep::Provider(Some(auth_type))),
            None => pending_options.map(|options| LoginBackStep::AuthType(Some(options))),
        };
        self.start_provider_login(&provider, back);
    }

    /// A plain option confirmed from the auth-type menu
    /// (`showLoginAuthTypeSelector`'s `onSelect`,
    /// `interactive-mode.ts:6122-6148`); `pending_options` is the one-provider
    /// entry list when the menu was scoped to a single provider.
    fn handle_auth_type_option(
        &mut self,
        option: &str,
        pending_options: Option<Vec<AuthSelectorProvider>>,
    ) {
        let all_options = oauth::login_provider_options(&self.auth_path);
        let radius = all_options
            .iter()
            .find(|provider| {
                provider.id == oauth::RADIUS_PROVIDER_ID && provider.auth_type == "oauth"
            })
            .cloned();
        let radius_label = radius.as_ref().map(|provider| {
            format!(
                "Sign in with {}{}",
                provider.name,
                Self::auth_selector_status_suffix(provider)
            )
        });
        if let (Some(radius), Some(radius_label)) = (&radius, &radius_label)
            && option == radius_label.as_str()
        {
            let back = pending_options.map(|options| LoginBackStep::AuthType(Some(options)));
            self.start_provider_login(radius, back);
            return;
        }
        let auth_type: &'static str = if option == "Sign in with an API key" {
            "api_key"
        } else {
            "oauth"
        };
        if let Some(provider_options) = &pending_options
            && let Some(provider) = provider_options
                .iter()
                .find(|provider| provider.auth_type == auth_type)
        {
            let provider = provider.clone();
            let back = Some(LoginBackStep::AuthType(pending_options));
            self.start_provider_login(&provider, back);
            return;
        }
        self.show_login_provider_selector(Some(auth_type), None);
    }

    /// A logout row confirmed (`showOAuthSelector`'s onSelect,
    /// `interactive-mode.ts:6220-6246`): delete the credential and report.
    fn logout_provider(&mut self, provider_id: &str, auth_type: &'static str) {
        let name = oauth::logout_provider_options(&self.auth_path)
            .ok()
            .and_then(|options| {
                options
                    .into_iter()
                    .find(|provider| provider.id == provider_id)
                    .map(|provider| provider.name)
            })
            .unwrap_or_else(|| provider_id.to_string());
        let storage = crate::auth::AuthStorage::create(&self.auth_path);
        match storage.delete(provider_id) {
            Ok(()) => {
                let message = if auth_type == "oauth" {
                    format!("Logged out of {name}")
                } else {
                    format!(
                        "Removed stored API key for {name}. Environment variables and models.json config are unchanged."
                    )
                };
                self.show_status(&message);
            }
            Err(error) => self.show_error(&format!("Logout failed: {error}")),
        }
    }

    /// One loop iteration without terminal input: editor events, agent
    /// events, spinner frames. `run` calls this between input polls; the
    /// scripted tests drive it directly with a capture terminal.
    #[allow(clippy::too_many_lines)] // mirrors the TS event loop shape; split when extending
    fn pump(&mut self) {
        // Editor events: take them out before acting on them, so the
        // mutex guard does not pin `self`.
        let editor_events = std::mem::take(&mut *self.editor_events.lock().expect("editor events"));
        for event in editor_events {
            match event {
                EditorEventOut::Submit(text) => {
                    self.editor_mut().editor.set_text("");
                    self.submit(&text);
                }
                EditorEventOut::Action(action) => self.handle_action(&action),
            }
        }

        // Selector events, drained through the Arcs the components publish
        // to (`showSelector`'s onSelect/onCancel callbacks).
        self.pump_model_selector();
        self.pump_trust_selector();
        self.pump_settings_selector();
        self.pump_session_selector();
        self.pump_auth_selector();
        self.pump_login_prompt();

        // The 3D easter egg (`playEasterEgg3d`): the header's logo clicks
        // replay from the queue, the animation's rows track the terminal, and
        // the completed exit hides the overlay (upstream's interval `finish`).
        let pending_clicks = match self.easter_egg_requests.lock() {
            Ok(mut requests) => std::mem::take(&mut *requests),
            Err(_) => Vec::new(),
        };
        for (column, row) in pending_clicks {
            self.play_easter_egg(EasterEgg3d::PiLogo {
                column: usize::try_from(column).unwrap_or(0),
                row: usize::try_from(row).unwrap_or(0),
            });
        }
        if let Some(handle) = &self.easter_egg
            && let Ok(size) = self.tui.terminal().size()
        {
            handle.rows.store(size.rows, Ordering::SeqCst);
        }
        if self
            .easter_egg
            .as_ref()
            .is_some_and(|handle| handle.done.load(Ordering::SeqCst))
        {
            if let Some(handle) = self.easter_egg.take() {
                self.tui.hide_overlay(handle.overlay);
                self.tui.request_render();
            }
        }

        // Agent events, without blocking the input loop: collect first,
        // then handle, so the run borrow ends before the handlers run.
        let mut incoming: Vec<AgentEvent> = Vec::new();
        let mut agent_done = false;
        if let Some(run) = &mut self.agent_run {
            loop {
                match run.events.try_recv() {
                    Ok(LoopEvent::Event(event)) => incoming.push(event),
                    Ok(LoopEvent::Done) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        agent_done = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                }
            }
        }
        for event in &incoming {
            self.handle_agent_event(event);
        }
        if agent_done {
            if let Some(run) = self.agent_run.take() {
                if let Some(handle) = run.handle {
                    let _ = handle.join();
                }
                // Router state decided this turn lands on the branch now
                // (upstream appends mid-turn; the store is not shared with
                // the loop thread).
                #[cfg(feature = "ext-js")]
                if let Some(routing) = &run.routing {
                    routing.persist_state(&mut self.session);
                }
            }
            self.set_working_status(None);
            self.tui.request_render();
        }

        // Compaction outcomes, collected first for the same borrow reason
        // (`compaction_end` events). The run is retired before the handlers
        // run: upstream clears the compaction promise before
        // `flushCompactionQueue`, and the flushed message re-enters
        // `submit`, whose guard must not see a live compaction.
        let mut compaction_outcomes: Vec<CompactionOutcome> = Vec::new();
        let mut compaction_finished = false;
        if let Some(run) = &mut self.compaction_run {
            loop {
                match run.events.try_recv() {
                    Ok(outcome) => compaction_outcomes.push(outcome),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        compaction_finished = true;
                        break;
                    }
                }
            }
        }
        if compaction_finished {
            if let Some(run) = self.compaction_run.take()
                && let Some(handle) = run.handle
            {
                let _ = handle.join();
            }
            self.set_working_status(None);
            self.tui.request_render();
        }
        for outcome in compaction_outcomes {
            self.handle_compaction_outcome(outcome);
        }

        // Terminal replies and the theme controller's deadline
        // (`tui.ts:1069-1071`; the controller runs on the UI thread).
        self.pump_theme();

        // Spinner frames (host-driven, `Loader::tick`).
        if self.last_spinner_tick.elapsed() >= Duration::from_millis(80) {
            self.last_spinner_tick = Instant::now();
            let mut dirty = false;
            if let Some(indicator) = self.status_handle.lock().expect("status").as_mut() {
                dirty |= indicator.tick();
            }
            if dirty {
                self.tui.request_render();
            }
        }
    }

    /// App actions from the editor (`setupKeyHandlers`, `handleCtrlC`).
    fn handle_action(&mut self, action: &AppAction) {
        match action {
            AppAction::Clear => {
                // Clear the editor; twice within 500ms exits.
                let now = Instant::now();
                if self
                    .last_sigint_time
                    .is_some_and(|at| now.duration_since(at) < Duration::from_millis(500))
                {
                    self.exiting = true;
                } else {
                    self.editor_mut().editor.set_text("");
                    self.last_sigint_time = Some(now);
                }
            }
            AppAction::Exit => self.exiting = true,
            AppAction::Interrupt => {
                // While compacting, interrupt cancels the compaction
                // (`CompactionStatusIndicator`'s cancel hint + the escape
                // override at `interactive-mode.ts:3810-3813`).
                if let Some(run) = &self.compaction_run {
                    run.abort.abort();
                } else if let Some(run) = &self.agent_run {
                    run.abort.abort();
                }
            }
            AppAction::CycleModel(direction) => self.cycle_model(*direction),
            AppAction::ToggleThinking => {
                self.hide_thinking_block = !self.hide_thinking_block;
                self.tui.request_render();
            }
            AppAction::FollowUp => {
                // `handleFollowUp` (`interactive-mode.ts:4655-4688`): queue
                // while streaming, otherwise act like Enter.
                let text = self.editor_mut().editor.get_text().trim().to_string();
                if text.is_empty() {
                    return;
                }
                self.editor_mut().editor.set_text("");
                if self.compaction_run.is_some() {
                    self.queue_compaction_message(&text, true);
                } else if self.agent_run.is_some() {
                    self.queue_message(&text, true);
                } else {
                    self.submit(&text);
                }
            }
            AppAction::Dequeue => self.dequeue_queued(),
            AppAction::ToolsExpand => {
                self.tool_output_expanded = !self.tool_output_expanded;
                let expanded = self.tool_output_expanded;
                let len = self.chat_len();
                for index in 0..len {
                    if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(index) {
                        component.set_expanded(expanded);
                    }
                    if let Some(component) =
                        self.downcast_chat::<super::components::compaction_summary::CompactionSummaryMessageComponent>(index)
                    {
                        component.set_expanded(expanded);
                    }
                }
                if let Some(header) = self.downcast_tree::<BuiltInHeader>(HostSlot::Header) {
                    header.set_expanded(expanded);
                }
                self.tui.request_render();
            }
        }
    }

    /// `shutdown` (`interactive-mode.ts:4475`): restore the terminal, print
    /// the resume command (`formatResumeCommand`). The fullscreen exits run
    /// through `stopInteractiveTui` (`interactive-mode.ts:632-639`): the
    /// transcript output drains the overlays, switches to the regular
    /// renderer, and lets the stop repaint the transcript into the
    /// scrollback; the resume-hint output leaves the alt screen in place and
    /// hands the terminal back untouched.
    fn shutdown(&mut self) -> Result<(), String> {
        let session_id = self.session.get_session_id().to_string();
        let uses_default_dir = self.session.uses_default_session_dir();
        let session_dir = self.session.get_session_dir().to_string_lossy().to_string();
        if self.tui.alt_screen_active()
            && self.settings.get_fullscreen_exit_output() == FullscreenExitOutput::Transcript
        {
            self.drain_overlays_for_exit();
            self.switch_tui_mode(TuiMode::Regular);
        }
        // `{ preserveScreen: this.renderer.mode === "fullscreen" }` (`:639`).
        let preserve_screen = self.tui.alt_screen_active();
        self.tui
            .stop(TuiStopOptions { preserve_screen })
            .map_err(|error| format!("terminal restore failed: {error}"))?;
        let mut command = format!("pi --session {session_id}");
        if !uses_default_dir {
            command = format!("pi --session-dir {session_dir} --session {session_id}");
        }
        let _ = writeln!(
            std::io::stdout(),
            "\x1b[2mTo resume this session:\x1b[22m {command}"
        );
        Ok(())
    }
}

/// `runInteractiveMode`: the CLI entry point.
/// `getNextCycleIndex` (`agent-session.ts:398-409`): when the current model
/// is not in the list, forward picks the first entry and backward the last,
/// so a one-entry list still selects its only model.
fn next_cycle_index(current_index: isize, length: usize, forward: bool) -> Option<usize> {
    if length == 0 {
        return None;
    }
    if current_index == -1 {
        return Some(if forward { 0 } else { length - 1 });
    }
    if length == 1 {
        return None;
    }
    let length = length as isize;
    let next = if forward {
        (current_index + 1) % length
    } else {
        (current_index - 1 + length) % length
    };
    Some(next as usize)
}

/// One cycle step over a model list (`_cycleAvailableModel`): the next model
/// in `direction`, or none when there is nothing to move to.
fn cycle_next(
    models: &[pi_ai::types::Model],
    current_provider: &str,
    current_id: &str,
    direction: i32,
) -> Option<pi_ai::types::Model> {
    let current_index = models
        .iter()
        .position(|m| m.provider == current_provider && m.id == current_id)
        .map_or(-1, |index| index as isize);
    let next_index = next_cycle_index(current_index, models.len(), direction > 0)?;
    Some(models[next_index].clone())
}

pub fn run_interactive_mode(
    runtime: &mut PrintRuntime,
    options: &InteractiveOptions,
) -> Result<i32, String> {
    let mut mode = InteractiveMode::new(runtime, options, Box::new(CrosstermTerminal::default()));
    mode.run()?;
    // Quit teardown (`runtimeHost.dispose`, `agent-session-runtime.ts:
    // 415-421`): `session_shutdown` with reason "quit" fires before the
    // process exits.
    mode.emit_extension_shutdown("quit", None);
    Ok(0)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

/// `keyDisplayText` (`keybinding-hints.js:27-29`) reduced to one key: the
/// action's first key, capitalized ("end" → "End").
pub(crate) fn key_display_text(action: &str) -> String {
    let Some(key) = pi_tui::keybindings::keys_for(action).first() else {
        return String::new();
    };
    let mut chars = key.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The `TuiMode` → its settings string.
fn tui_mode_as_setting(mode: TuiMode) -> &'static str {
    match mode {
        TuiMode::Regular => "regular",
        TuiMode::Fullscreen => "fullscreen",
    }
}

/// The resolved wheel setting → its row value (`String(...)` in
/// `settings-selector.ts:518`).
fn wheel_lines_as_setting(lines: pi_tui::wheel_scroll::WheelScrollLines) -> String {
    match lines {
        pi_tui::wheel_scroll::WheelScrollLines::Auto => "auto".to_string(),
        pi_tui::wheel_scroll::WheelScrollLines::Fixed(lines) => lines.to_string(),
    }
}

/// The wheel-scrolling row value → the setting, with the getter's validation
/// (`getFullscreenWheelScrollLines`, `settings-manager.js:1041-1049`):
/// `"auto"`, or whole lines clamped to 1-100.
fn fullscreen_wheel_lines(value: &str) -> pi_tui::wheel_scroll::WheelScrollLines {
    match value.parse::<f64>() {
        Ok(lines) if lines.is_finite() => {
            pi_tui::wheel_scroll::WheelScrollLines::Fixed(lines.clamp(1.0, 100.0).floor())
        }
        _ => pi_tui::wheel_scroll::WheelScrollLines::Auto,
    }
}

/// The cached git branch for the footer.
fn git_branch(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

#[cfg(test)]
#[cfg_attr(test, allow(clippy::assert_is_empty))]
mod tests {
    use super::*;
    use pi_ai::types::{AssistantMessageEvent, StopReason, TextContent, ToolCall};
    use pi_tui::ansi::strip_terminal_sequences;
    use pi_tui::terminal::{CaptureTerminal, FrameCapture, TerminalSize};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::session::records::SessionEntry;
    use crate::tools::index::ToolRegistry;

    /// One `AssistantMessage` for the scripted stream, with the fields the
    /// wire always carries (the renderers read usage and stop reason).
    fn scripted_assistant(
        content: &[pi_ai::types::AssistantContent],
        stop: StopReason,
    ) -> pi_ai::types::AssistantMessage {
        serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": serde_json::to_value(content).expect("content"),
            "api": "test",
            "provider": "test",
            "model": "test-model",
            "usage": {
                "input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 15,
                "cost": { "input": 0.001, "output": 0.002, "cacheRead": 0.0,
                          "cacheWrite": 0.0, "total": 0.003 }
            },
            "stopReason": stop.as_str(),
            "timestamp": 0
        }))
        .expect("assistant message")
    }

    /// The scripted model: turn 0 answers with text plus a `read` call, turn 1
    /// summarizes the result. Everything downstream of `stream` — the agent
    /// loop, the real `read` execution, the renderers, the render tree — is
    /// the production code path.
    struct ScriptedStream {
        calls: AtomicUsize,
        file_path: String,
    }

    impl pi_agent::types::StreamFn for ScriptedStream {
        fn stream(
            &self,
            _request: pi_agent::types::StreamRequest,
        ) -> futures::stream::BoxStream<'static, AssistantMessageEvent> {
            let turn = self.calls.fetch_add(1, Ordering::SeqCst);
            if turn == 0 {
                // Hold the stream open long enough for the first render to
                // catch the working status in the editor border.
                std::thread::sleep(Duration::from_millis(150));
                let tool_call: ToolCall = serde_json::from_value(serde_json::json!({
                    "id": "call-1",
                    "name": "read",
                    "arguments": { "path": self.file_path }
                }))
                .expect("tool call");
                let partial = scripted_assistant(
                    &[
                        pi_ai::types::AssistantContent::Text(TextContent {
                            text: "Let me check the file.".to_string(),
                            text_signature: None,
                        }),
                        pi_ai::types::AssistantContent::ToolCall(tool_call.clone()),
                    ],
                    StopReason::ToolUse,
                );
                Box::pin(futures::stream::iter(vec![
                    AssistantMessageEvent::Start {
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::TextStart {
                        content_index: 0,
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::ToolcallStart {
                        content_index: 1,
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::ToolcallEnd {
                        content_index: 1,
                        tool_call,
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::Done {
                        reason: StopReason::ToolUse,
                        message: partial,
                    },
                ]))
            } else {
                let partial = scripted_assistant(
                    &[pi_ai::types::AssistantContent::Text(TextContent {
                        text: "The file says: hello from pi-rs".to_string(),
                        text_signature: None,
                    })],
                    StopReason::Stop,
                );
                Box::pin(futures::stream::iter(vec![
                    AssistantMessageEvent::Start {
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::TextStart {
                        content_index: 0,
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::TextEnd {
                        content_index: 0,
                        content: "The file says: hello from pi-rs".to_string(),
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: partial,
                    },
                ]))
            }
        }
    }

    /// A capture-terminal mode over a read-only executor in `dir`.
    fn fixture(dir: &std::path::Path, file_path: &str) -> (InteractiveMode, FrameCapture) {
        let stream: Arc<dyn pi_agent::types::StreamFn> = Arc::new(ScriptedStream {
            calls: AtomicUsize::new(0),
            file_path: file_path.to_string(),
        });
        fixture_with_stream(dir, stream)
    }

    /// A capture-terminal mode over a read-only executor in `dir`, driven by a
    /// caller-supplied stream.
    /// The runtime behind the fixtures: a scripted model over a read-only
    /// executor in `dir`.
    fn test_runtime(
        dir: &std::path::Path,
        stream: Arc<dyn pi_agent::types::StreamFn>,
    ) -> PrintRuntime {
        let model = pi_ai::types::Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "test".to_string(),
            provider: "test".to_string(),
            reasoning: true,
            context_window: 200_000,
            max_tokens: 8192,
            ..pi_ai::types::Model::default()
        };
        PrintRuntime {
            model,
            thinking_level: ThinkingLevel::default(),
            api_key: Some("test".to_string()),
            access_token: None,
            stream_fn: stream,
            executor: ToolRegistry::read_only().executor(dir),
            system_prompt: "test".to_string(),
            session: SessionManager::in_memory(dir, &NewSessionOptions::default(), Vec::new())
                .expect("session"),
            project_trusted: false,
            retry_settings: crate::config::settings::RetrySettings {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 2000,
                max_agent_delay_ms: 60_000,
            },
            compaction_settings: crate::session::compaction::DEFAULT_COMPACTION_SETTINGS,
            #[cfg(feature = "ext-js")]
            extension_runner: None,
        }
    }

    fn fixture_with_stream(
        dir: &std::path::Path,
        stream: Arc<dyn pi_agent::types::StreamFn>,
    ) -> (InteractiveMode, FrameCapture) {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 100,
            rows: 40,
        });
        let capture = terminal.capture();
        let options = InteractiveOptions {
            auth_path: Some(dir.join("auth.json")),
            ..InteractiveOptions::default()
        };
        let mut runtime = test_runtime(dir, stream);
        let mode = InteractiveMode::new(&mut runtime, &options, Box::new(terminal));
        (mode, capture)
    }

    /// A mode over a capture terminal whose response store the test still
    /// holds: replies pushed after construction drive the theme wire exactly
    /// like a terminal answering the mode's queries.
    fn themed_fixture(dir: &std::path::Path) -> (InteractiveMode, CaptureTerminal) {
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 100,
            rows: 40,
        });
        let handle = terminal.clone();
        let options = InteractiveOptions {
            auth_path: Some(dir.join("auth.json")),
            ..InteractiveOptions::default()
        };
        let mut runtime = test_runtime(
            dir,
            Arc::new(ScriptedStream {
                calls: AtomicUsize::new(0),
                file_path: "unused".to_string(),
            }),
        );
        let mode = InteractiveMode::new(&mut runtime, &options, Box::new(terminal));
        (mode, handle)
    }

    #[test]
    fn resumed_session_replays_its_transcript() {
        use pi_ai::types::{ToolResultMessage, UserContent, UserMessage};

        let dir = tempfile::tempdir().expect("tempdir");
        // A persisted session with a full round trip: user prompt, assistant
        // tool call, tool result, final answer.
        let mut session = SessionManager::create(dir.path(), None, &NewSessionOptions::default())
            .expect("session");
        session
            .append_message(AgentMessage::User(UserMessage {
                content: UserContent::Text("list the files".to_string()),
                timestamp: 0,
            }))
            .expect("append user");
        let assistant_call: pi_ai::types::AssistantMessage =
            serde_json::from_value(serde_json::json!({
                "content": [{
                    "type": "toolCall", "id": "call-1", "name": "ls",
                    "arguments": {"path": "."}
                }],
                "api": "test", "provider": "test", "model": "test-model",
                "timestamp": 0,
                "usage": {"input": 5, "output": 3, "cacheRead": 0, "cacheWrite": 0,
                          "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}},
                "stopReason": "toolUse"
            }))
            .expect("assistant call");
        session
            .append_message(AgentMessage::Assistant(assistant_call))
            .expect("append call");
        let result: ToolResultMessage = serde_json::from_value(serde_json::json!({
            "toolCallId": "call-1", "toolName": "ls",
            "content": [{"type": "text", "text": "note.txt"}],
            "isError": false, "timestamp": 0
        }))
        .expect("tool result");
        session
            .append_message(AgentMessage::ToolResult(result))
            .expect("append result");
        let assistant_final: pi_ai::types::AssistantMessage =
            serde_json::from_value(serde_json::json!({
                "content": [{"type": "text", "text": "done listing"}],
                "api": "test", "provider": "test", "model": "test-model",
                "timestamp": 0,
                "usage": {"input": 5, "output": 3, "cacheRead": 0, "cacheWrite": 0,
                          "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}},
                "stopReason": "stop"
            }))
            .expect("assistant final");
        session
            .append_message(AgentMessage::Assistant(assistant_final))
            .expect("append final");
        let path = session.session_file().expect("session file").to_path_buf();
        drop(session);

        // Reopen it and render the transcript into a capture terminal.
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 100,
            rows: 40,
        });
        let capture = terminal.capture();
        let options = InteractiveOptions {
            auth_path: Some(dir.path().join("auth.json")),
            ..InteractiveOptions::default()
        };
        let stream: Arc<dyn pi_agent::types::StreamFn> = Arc::new(ScriptedStream {
            calls: AtomicUsize::new(0),
            file_path: "unused".to_string(),
        });
        let mut runtime = test_runtime(dir.path(), stream);
        runtime.session = SessionManager::open(&path, None, None).expect("reopen");
        let mut mode = InteractiveMode::new(&mut runtime, &options, Box::new(terminal));
        mode.tui.start().expect("tui start");
        mode.render_initial_messages();
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("list the files"), "{plain}");
        // The tool row rendered with its replayed result.
        assert!(plain.contains("note.txt"), "{plain}");
        assert!(plain.contains("done listing"), "{plain}");
    }

    #[test]
    fn end_to_end_tool_call_round_trip_renders_like_pi() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello from pi-rs\n").expect("write");
        let (mut mode, capture) = fixture(dir.path(), &file.to_string_lossy());
        mode.tui.start().expect("tui start");

        // Submit through the editor-event queue — the path a keystroke takes.
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Submit("what is in the file?".to_string()));
        for _ in 0..400 {
            mode.pump();
            mode.tui.render_now(false);
            if mode.agent_run.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(mode.agent_run.is_none(), "agent run did not finish");
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        // The startup header with its compact hints.
        assert!(
            plain.contains("Press ctrl+o to show full startup help."),
            "{plain}"
        );
        // The user prompt and both assistant turns.
        assert!(plain.contains("what is in the file?"), "{plain}");
        assert!(plain.contains("Let me check the file."), "{plain}");
        assert!(plain.contains("The file says: hello from pi-rs"), "{plain}");
        // The tool row: the call line names the file, the result shows it.
        assert!(plain.contains("note.txt"), "{plain}");
        assert!(plain.contains("hello from pi-rs"), "{plain}");
        // The footer shows the model and the accumulated usage totals —
        // both assistant turns' usage summed by `refresh_footer`.
        assert!(plain.contains("test-model"), "{plain}");
        assert!(plain.contains("↑20"), "{plain}");
        // The transcript persisted through the real session path.
        let entries = mode.session.get_entries();
        assert!(
            entries
                .iter()
                .any(|entry| matches!(entry, SessionEntry::Message(_))),
            "no persisted messages"
        );
    }

    #[test]
    fn working_status_rides_the_editor_border_mid_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "content\n").expect("write");
        let (mut mode, capture) = fixture(dir.path(), &file.to_string_lossy());
        mode.tui.start().expect("tui start");
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Submit("go".to_string()));
        // First pump starts the run; the scripted stream sleeps 150ms, so
        // this render is guaranteed to catch the status.
        mode.pump();
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Working..."), "{plain}");
        // Drain the run so the test does not leak a worker thread.
        for _ in 0..400 {
            mode.pump();
            if mode.agent_run.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(mode.agent_run.is_none(), "agent run did not finish");
    }

    // --------------------------------------------------------------
    // The OSC theme controller (`theme/theme-controller.ts`): the replies a
    // real terminal sends to the mode's queries re-theme it like pi.
    // --------------------------------------------------------------

    #[test]
    fn osc_color_reply_rethemes_the_system_theme() {
        let _guard = theme::COLOR_STATE_LOCK.lock().expect("color state lock");
        theme::reset_terminal_color_state();
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, terminal) = themed_fixture(dir.path());
        mode.tui.start().expect("tui start");

        // White on black — what a dark terminal answers to the OSC 10/11
        // query — closed by the DA1 fence.
        terminal.push_response("\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
        terminal.push_response("\x1b]11;rgb:0000/0000/0000\x1b\\");
        terminal.push_response("\x1b[?1;2c");
        mode.pump();
        mode.pump();
        // The black background resolved the system theme.
        assert_eq!(theme::terminal_theme(), "dark");
        // A later unrequested reply is swallowed by the parser, not treated
        // as keystrokes, and leaves the theme alone.
        terminal.push_response("\x1b]11;rgb:0000/0000/0000\x1b\\");
        mode.pump();
        assert_eq!(theme::terminal_theme(), "dark");

        theme::reset_terminal_color_state();
    }

    #[test]
    fn late_color_reply_after_the_deadline_still_applies() {
        let _guard = theme::COLOR_STATE_LOCK.lock().expect("color state lock");
        theme::reset_terminal_color_state();
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, terminal) = themed_fixture(dir.path());
        mode.tui.start().expect("tui start");

        // The initial query outlives its 100 ms budget with nothing recorded:
        // the deadline arms the query's onLateReply sink.
        std::thread::sleep(Duration::from_millis(150));
        mode.pump();
        // Replies that arrive after the settlement ride the sink into the
        // same apply path.
        terminal.push_response("\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
        terminal.push_response("\x1b]11;rgb:0000/0000/0000\x1b\\");
        terminal.push_response("\x1b[?1;2c");
        mode.pump();
        assert_eq!(theme::terminal_theme(), "dark");

        theme::reset_terminal_color_state();
    }

    #[test]
    fn mode_2031_scheme_report_applies_only_when_auto_sync() {
        let _guard = theme::COLOR_STATE_LOCK.lock().expect("color state lock");
        theme::reset_terminal_color_state();
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, terminal) = themed_fixture(dir.path());
        mode.tui.start().expect("tui start");

        // The default setting (system) auto-syncs, so the report records.
        assert!(mode.theme_auto_sync);
        terminal.push_response("\x1b[?997;2n");
        mode.pump();
        assert_eq!(
            theme::terminal_scheme_for_tests(),
            Some(ThemeAppearance::Light)
        );
        // With auto-sync off the report is ignored entirely.
        mode.theme_auto_sync = false;
        terminal.push_response("\x1b[?997;1n");
        mode.pump();
        assert_eq!(
            theme::terminal_scheme_for_tests(),
            Some(ThemeAppearance::Light),
            "auto-sync off ignores the report"
        );

        theme::reset_terminal_color_state();
    }

    /// The canned summary the content-aware stream returns for a
    /// summarization request.
    const CANNED_SUMMARY: &str =
        "## Goal\ncompact the file chat\n\n## Context\nThe user asked what is in the file.";

    /// Content-aware scripted model: a compaction request (the summarization
    /// system prompt riding the stream seam) gets a canned summary; normal
    /// turns behave like `ScriptedStream`. The agent loop, the real `read`
    /// execution, the compaction pipeline, and the renderers are all the
    /// production code path.
    struct CompactingStream {
        calls: AtomicUsize,
        file_path: String,
    }

    impl CompactingStream {
        fn summarization_events() -> Vec<AssistantMessageEvent> {
            let partial = scripted_assistant(
                &[pi_ai::types::AssistantContent::Text(TextContent {
                    text: CANNED_SUMMARY.to_string(),
                    text_signature: None,
                })],
                StopReason::Stop,
            );
            vec![
                AssistantMessageEvent::Start {
                    partial: partial.clone(),
                },
                AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: partial.clone(),
                },
                AssistantMessageEvent::TextEnd {
                    content_index: 0,
                    content: CANNED_SUMMARY.to_string(),
                    partial: partial.clone(),
                },
                AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: partial,
                },
            ]
        }
    }

    impl pi_agent::types::StreamFn for CompactingStream {
        fn stream(
            &self,
            request: pi_agent::types::StreamRequest,
        ) -> futures::stream::BoxStream<'static, AssistantMessageEvent> {
            let summarizing = match request.messages.first() {
                Some(AgentMessage::System(system)) => matches!(
                    &system.content,
                    pi_ai::types::StringOrTextBlocks::Text(text)
                        if text.starts_with("You are a context summarization assistant")
                ),
                _ => false,
            };
            if summarizing {
                // Hold the request open so the test can observe the status and
                // queue a message mid-compaction.
                std::thread::sleep(Duration::from_millis(150));
                return Box::pin(futures::stream::iter(Self::summarization_events()));
            }
            let turn = self.calls.fetch_add(1, Ordering::SeqCst);
            if turn == 0 {
                std::thread::sleep(Duration::from_millis(150));
                let tool_call: ToolCall = serde_json::from_value(serde_json::json!({
                    "id": "call-1",
                    "name": "read",
                    "arguments": { "path": self.file_path }
                }))
                .expect("tool call");
                let partial = scripted_assistant(
                    &[
                        pi_ai::types::AssistantContent::Text(TextContent {
                            text: "Let me check the file.".to_string(),
                            text_signature: None,
                        }),
                        pi_ai::types::AssistantContent::ToolCall(tool_call.clone()),
                    ],
                    StopReason::ToolUse,
                );
                Box::pin(futures::stream::iter(vec![
                    AssistantMessageEvent::Start {
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::ToolcallStart {
                        content_index: 1,
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::ToolcallEnd {
                        content_index: 1,
                        tool_call,
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::Done {
                        reason: StopReason::ToolUse,
                        message: partial,
                    },
                ]))
            } else {
                let partial = scripted_assistant(
                    &[pi_ai::types::AssistantContent::Text(TextContent {
                        text: "The file says: hello from pi-rs".to_string(),
                        text_signature: None,
                    })],
                    StopReason::Stop,
                );
                Box::pin(futures::stream::iter(vec![
                    AssistantMessageEvent::Start {
                        partial: partial.clone(),
                    },
                    AssistantMessageEvent::Done {
                        reason: StopReason::Stop,
                        message: partial,
                    },
                ]))
            }
        }
    }

    /// A capture-terminal mode driven by `CompactingStream`.
    fn compact_fixture(dir: &std::path::Path, file_path: &str) -> (InteractiveMode, FrameCapture) {
        let stream: Arc<dyn pi_agent::types::StreamFn> = Arc::new(CompactingStream {
            calls: AtomicUsize::new(0),
            file_path: file_path.to_string(),
        });
        fixture_with_stream(dir, stream)
    }

    /// Submit through the editor-event queue — the path a keystroke takes —
    /// and wait for the agent run to settle.
    fn run_to_completion(mode: &mut InteractiveMode, text: &str) {
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Submit(text.to_string()));
        for _ in 0..400 {
            mode.pump();
            if mode.agent_run.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("run did not finish");
    }

    /// The budget-busting prompt: the keep-recent budget is 20000 tokens
    /// (≈80k chars) and the read tool caps its result at 50KB, so the second
    /// user turn is what pushes the backward walk past the budget.
    fn big_prompt() -> String {
        format!("please remember this: {}", "x".repeat(100_000))
    }

    #[test]
    fn compact_command_summarizes_persists_and_reports_the_cost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello from pi-rs\n").expect("write");
        let (mut mode, capture) = compact_fixture(dir.path(), &file.to_string_lossy());
        mode.tui.start().expect("tui start");

        // Two normal turns, so the cut has room before the budget overrun.
        run_to_completion(&mut mode, "what is in the file?");
        run_to_completion(&mut mode, &big_prompt());

        // /compact starts a compaction run with its own status indicator.
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Submit("/compact".to_string()));
        mode.pump();
        mode.tui.render_now(false);
        assert!(mode.compaction_run.is_some(), "compaction did not start");
        let mid_flight = strip_terminal_sequences(&capture.output());
        assert!(
            mid_flight.contains("Compacting context... (ctrl+c to cancel)"),
            "{mid_flight}"
        );

        // The outcome lands as chat rows through the real session path.
        for _ in 0..400 {
            mode.pump();
            mode.tui.render_now(false);
            if mode.compaction_run.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(mode.compaction_run.is_none(), "compaction did not finish");
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("[compaction]"), "{plain}");
        assert!(plain.contains("Compacted from"), "{plain}");
        // The billed-tokens notice: input 10 + output 5 from the scripted usage.
        assert!(plain.contains("Compaction: 15 tokens billed"), "{plain}");

        // The summary persisted as a compaction entry.
        let entries = mode.session.get_entries();
        let compaction = entries
            .iter()
            .find_map(|entry| match entry {
                SessionEntry::Compaction(compaction) => Some(compaction),
                _ => None,
            })
            .expect("no persisted compaction entry");
        // The summary rides with the read-files appendix built from the real
        // `read` tool call.
        assert_eq!(
            compaction.summary,
            format!(
                "{CANNED_SUMMARY}\n\n<read-files>\n{path}\n</read-files>",
                path = file.to_string_lossy()
            )
        );

        // A second /compact is rejected: the branch ends in a compaction.
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Submit("/compact".to_string()));
        mode.pump();
        mode.tui.render_now(false);
        assert!(
            mode.compaction_run.is_none(),
            "double compact started a run"
        );
        let plain = strip_terminal_sequences(&capture.output());
        assert!(
            plain.contains("Compaction failed: Already compacted"),
            "{plain}"
        );
    }

    #[test]
    fn messages_submitted_during_compaction_queue_then_flush() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello from pi-rs\n").expect("write");
        let (mut mode, capture) = compact_fixture(dir.path(), &file.to_string_lossy());
        mode.tui.start().expect("tui start");

        run_to_completion(&mut mode, "what is in the file?");
        run_to_completion(&mut mode, &big_prompt());

        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Submit("/compact".to_string()));
        mode.pump();
        assert!(mode.compaction_run.is_some(), "compaction did not start");

        // A submit while compaction runs queues instead of starting a run.
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Submit(
                "after compaction please".to_string(),
            ));
        mode.pump();
        mode.tui.render_now(false);
        assert!(mode.compaction_run.is_some(), "compaction ended early");
        assert!(mode.agent_run.is_none(), "queued message started a run");
        let mid_flight = strip_terminal_sequences(&capture.output());
        assert!(
            mid_flight.contains("Queued message for after compaction"),
            "{mid_flight}"
        );

        // The outcome flushes the queue: the queued message runs and answers.
        for _ in 0..400 {
            mode.pump();
            if mode.compaction_run.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        for _ in 0..400 {
            mode.pump();
            mode.tui.render_now(false);
            if mode.agent_run.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            mode.compaction_run.is_none() && mode.agent_run.is_none(),
            "flush run did not finish"
        );
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("[compaction]"), "{plain}");
        assert!(plain.contains("after compaction please"), "{plain}");

        // The flushed message persisted directly after the compaction entry.
        let entries = mode.session.get_entries();
        let compaction_index = entries
            .iter()
            .position(|entry| matches!(entry, SessionEntry::Compaction(_)))
            .expect("no compaction entry");
        let next = entries.get(compaction_index + 1);
        assert!(
            matches!(
                next,
                Some(SessionEntry::Message(entry))
                    if matches!(&entry.message, AgentMessage::User(user)
                        if matches!(&user.content,
                            pi_ai::types::UserContent::Text(text) if text.contains("after compaction please")))
            ),
            "queued message did not persist after the compaction: {next:?}"
        );
    }

    #[test]
    fn cycle_indices_follow_upstream_semantics() {
        // `getNextCycleIndex` (`agent-session.ts:398-409`).
        assert_eq!(next_cycle_index(0, 3, true), Some(1));
        assert_eq!(next_cycle_index(2, 3, true), Some(0));
        assert_eq!(next_cycle_index(0, 3, false), Some(2));
        assert_eq!(next_cycle_index(1, 1, true), None);
        assert_eq!(next_cycle_index(0, 0, true), None);
        // Current model missing from the list: forward → first, backward → last.
        assert_eq!(next_cycle_index(-1, 3, true), Some(0));
        assert_eq!(next_cycle_index(-1, 3, false), Some(2));
        assert_eq!(next_cycle_index(-1, 1, true), Some(0));
    }

    #[test]
    fn cycle_next_steps_through_the_list() {
        fn model(provider: &str, id: &str) -> pi_ai::types::Model {
            pi_ai::types::Model {
                provider: provider.to_string(),
                id: id.to_string(),
                name: String::new(),
                ..pi_ai::types::Model::default()
            }
        }
        let models = vec![model("a", "one"), model("a", "two"), model("b", "three")];
        let next = cycle_next(&models, "a", "one", 1).expect("forward");
        assert_eq!((next.provider.as_str(), next.id.as_str()), ("a", "two"));
        let previous = cycle_next(&models, "a", "one", -1).expect("backward");
        assert_eq!(
            (previous.provider.as_str(), previous.id.as_str()),
            ("b", "three")
        );
        // Wrap-around.
        let wrapped = cycle_next(&models, "b", "three", 1).expect("wrap");
        assert_eq!(
            (wrapped.provider.as_str(), wrapped.id.as_str()),
            ("a", "one")
        );
        // The current model absent from the list → forward selects the first.
        let fresh = cycle_next(&models, "zzz", "none", 1).expect("fresh");
        assert_eq!((fresh.provider.as_str(), fresh.id.as_str()), ("a", "one"));
        // Single-entry list → nothing to cycle.
        let single = vec![model("a", "one")];
        assert!(cycle_next(&single, "a", "one", 1).is_none());
        // Empty list.
        assert!(cycle_next(&[], "a", "one", 1).is_none());
    }

    #[test]
    fn model_selector_mounts_in_place_of_the_editor_and_restores_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        // `/model` with no pattern opens the selector in the editor slot.
        mode.submit("/model");
        mode.tui.render_now(false);
        assert!(mode.suspended_editor.is_some(), "selector not mounted");
        let selector = mode
            .downcast_tree::<ModelSelectorComponent>(HostSlot::Editor)
            .expect("selector in the editor slot");
        let rendered = pi_tui::ansi::strip_terminal_sequences(&selector.render(100).join("\n"));
        assert!(
            rendered.contains("Only showing models from configured providers"),
            "{rendered}"
        );

        // Selecting a model applies it and restores the editor.
        let model = pi_ai::types::Model {
            id: "test-model".to_string(),
            name: "Test Model".to_string(),
            api: "test".to_string(),
            provider: "test".to_string(),
            ..pi_ai::types::Model::default()
        };
        mode.model_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events lock")
            .push(ModelSelectorEvent::Selected(model));
        mode.pump();
        assert!(mode.suspended_editor.is_none(), "editor not restored");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Model: test-model"), "{frame}");
        let editor = mode
            .downcast_tree::<ChatEditor>(HostSlot::Editor)
            .expect("editor back");
        assert!(editor.has_input_handler(), "focus-able editor restored");

        // Escape cancels without touching the model.
        mode.submit("/model");
        mode.model_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events lock")
            .push(ModelSelectorEvent::Cancelled);
        mode.pump();
        assert!(mode.suspended_editor.is_none(), "editor not restored");
        assert_eq!(mode.model.id, "test-model");
    }

    #[test]
    fn trust_selector_saves_the_decision_and_restores_the_editor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let mut runtime = test_runtime(
            dir.path(),
            Arc::new(ScriptedStream {
                calls: AtomicUsize::new(0),
                file_path: "unused".to_string(),
            }),
        );
        let options = InteractiveOptions {
            auth_path: Some(dir.path().join("auth.json")),
            agent_dir: Some(agent_dir.path().to_path_buf()),
            ..InteractiveOptions::default()
        };
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 100,
            rows: 40,
        });
        let capture = terminal.capture();
        let mut mode = InteractiveMode::new(&mut runtime, &options, Box::new(terminal));
        mode.tui.start().expect("tui start");

        // `/trust` opens the selector in the editor slot.
        mode.submit("/trust");
        mode.tui.render_now(false);
        assert!(mode.suspended_editor.is_some(), "selector not mounted");
        let selector = mode
            .downcast_tree::<TrustSelectorComponent>(HostSlot::Editor)
            .expect("selector in the editor slot");
        let rendered = strip_terminal_sequences(&selector.render(100).join("\n"));
        assert!(rendered.contains("Project trust"), "{rendered}");

        // Confirming the highlighted "Trust" option persists the decision to
        // the injected agent dir and reports the restart note.
        mode.trust_selector_events
            .as_ref()
            .expect("trust events")
            .lock()
            .expect("trust events lock")
            .push(TrustSelectorEvent::Select {
                trusted: true,
                updates: crate::cli::project_trust::project_trust_options(dir.path())[0]
                    .updates
                    .clone(),
            });
        mode.pump();
        assert!(mode.suspended_editor.is_none(), "editor not restored");
        mode.tui.render_now(false);
        let frame = strip_terminal_sequences(&capture.output());
        assert!(
            frame.contains("Saved trust decision: trusted. Restart pi for this to take effect."),
            "{frame}"
        );
        let store = ProjectTrustStore::new(agent_dir.path());
        assert_eq!(store.get(dir.path()), Ok(Some(true)));
    }

    #[test]
    fn submitting_during_a_run_steers_and_persists_in_delivery_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("first question");
        for _ in 0..100 {
            mode.pump();
            if mode.agent_run.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(mode.agent_run.is_some(), "run did not start");

        // Plain Enter while streaming steers; the editor intercepts alt+enter
        // into the follow-up action.
        mode.submit("steered mid-run");
        mode.editor_mut().editor.set_text("follow-up mid-run");
        let mutation = Component::handle_input(mode.editor_mut(), "\x1b\r");
        assert!(mutation.is_some(), "alt+enter consumed");
        let events = mode.editor_events.lock().expect("editor events").clone();
        assert!(events.contains(&EditorEventOut::Action(AppAction::FollowUp)));

        mode.pump();
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Steering: steered mid-run"), "{frame}");
        assert!(frame.contains("Follow-up: follow-up mid-run"), "{frame}");

        for _ in 0..400 {
            mode.pump();
            if mode.agent_run.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(mode.agent_run.is_none(), "run did not finish");

        // Transcript order: the first prompt, then the queued messages in
        // delivery order (steering before follow-up).
        let user_texts: Vec<String> = mode
            .session
            .get_entries()
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(entry) => match &entry.message {
                    AgentMessage::User(user) => match &user.content {
                        pi_ai::types::UserContent::Text(text) => Some(text.clone()),
                        pi_ai::types::UserContent::Blocks(_) => None,
                    },
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            user_texts,
            vec![
                "first question".to_string(),
                "steered mid-run".to_string(),
                "follow-up mid-run".to_string()
            ]
        );

        // The pending display cleared once everything was delivered (the
        // capture stream keeps every frame, so assert on the live state).
        assert!(mode.queued_deliveries.is_empty());
        let pending = mode
            .downcast_tree::<pi_tui::component::Container>(HostSlot::Pending)
            .expect("pending container");
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn dequeue_restores_queued_messages_to_the_editor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("first question");
        for _ in 0..100 {
            mode.pump();
            if mode.agent_run.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        mode.submit("queued one");
        mode.editor_mut().editor.set_text("typed draft");
        mode.handle_action(&AppAction::Dequeue);

        // Queues cleared, editor carries both, pending display emptied.
        assert!(!mode.queues.lock().expect("queues").has_items());
        assert!(mode.queued_deliveries.is_empty());
        let text = mode.editor_mut().editor.get_text();
        assert_eq!(text, "queued one\n\ntyped draft");

        // With nothing queued, dequeue reports and does nothing.
        mode.handle_action(&AppAction::Dequeue);
        assert_eq!(
            mode.editor_mut().editor.get_text(),
            "queued one\n\ntyped draft"
        );

        for _ in 0..400 {
            mode.pump();
            if mode.agent_run.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn app_actions_expand_header_and_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");
        mode.tui.render_now(false);
        let compact = strip_terminal_sequences(&capture.output());
        assert!(compact.contains("clear/exit"), "{compact}");
        assert!(!compact.contains("to cycle thinking level"), "{compact}");

        // ctrl+o expands the header hints (and tool rows; none yet).
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Action(AppAction::ToolsExpand));
        mode.pump();
        mode.tui.render_now(false);
        let expanded = strip_terminal_sequences(&capture.output());
        assert!(expanded.contains("to cycle thinking level"), "{expanded}");

        // ctrl+d on an empty editor exits; the next pump breaks the loop.
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Action(AppAction::Exit));
        mode.pump();
        assert!(mode.exiting, "ctrl+d did not set the exit flag");
    }

    /// The fullscreen switch over the capture terminal: the viewport mounts
    /// on the alt screen with focus by identity, a mounted selector blocks
    /// the switch (`switchTuiMode`'s overlay guard, `interactive-mode.ts:645`),
    /// and the way back restores the flat tree.
    #[test]
    fn fullscreen_switch_enters_and_leaves_the_alt_screen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");
        assert_eq!(
            mode.tui_mode,
            TuiMode::Fullscreen,
            "fullscreen is the settings default"
        );
        // run() enters the resolved mode's alt screen; the test drives the
        // switch mechanics directly.
        assert!(
            mode.switch_tui_mode(TuiMode::Fullscreen),
            "an already-matched mode is a no-op success"
        );
        assert!(!mode.tui.alt_screen_active());
        mode.enter_fullscreen().expect("enter fullscreen");
        assert!(mode.tui.alt_screen_active());
        {
            let viewport = mode.viewport.as_ref().expect("viewport");
            assert_eq!(
                mode.tui.alt_screen_root(),
                Some(viewport.root_id()),
                "the viewport is the layout root"
            );
        }
        let entered = capture.output();
        assert!(
            entered.contains(pi_tui::tui_alt_screen::ENTER_ALT_SCREEN),
            "the alt screen was entered: {entered:?}"
        );

        // A selector occupies the editor dock slot and blocks the switch.
        mode.open_model_selector(None);
        assert!(
            !mode.switch_tui_mode(TuiMode::Regular),
            "the switch is refused while a selector is open"
        );
        assert!(mode.tui.alt_screen_active());
        mode.close_model_selector();

        assert!(mode.switch_tui_mode(TuiMode::Regular));
        assert!(!mode.tui.alt_screen_active());
        assert!(mode.viewport.is_none(), "the viewport is unmounted");
        let left = capture.output();
        assert!(
            left.contains(pi_tui::tui_alt_screen::EXIT_ALT_SCREEN),
            "the alt screen was left: {left:?}"
        );
        mode.tui.render_now(true);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(
            plain.contains("clear/exit"),
            "the flat tree renders again: {plain}"
        );

        mode.shutdown().expect("shutdown");
    }

    /// `/arminsayshi` in the flat tree: the 3D egg cannot play (no alt
    /// screen), so the inline effect lands in the chat
    /// (`handleArminSaysHi`'s fallback, `interactive-mode.ts:5993-5998`).
    #[test]
    fn arminsayshi_falls_back_to_the_inline_component_in_regular_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");
        assert!(mode.switch_tui_mode(TuiMode::Regular));

        mode.submit("/arminsayshi");
        mode.tui.render_now(false);
        assert!(mode.easter_egg.is_none(), "no overlay in the flat tree");
        assert!(!mode.tui.has_overlay());
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("ARMIN SAYS HI"), "{plain}");

        mode.shutdown().expect("shutdown");
    }

    /// `/arminsayshi` fullscreen: the 3D Armin plays as an overlay, and the
    /// pump retires it once the exit completes (upstream's `finish` clears
    /// the interval and hides the overlay).
    #[test]
    fn arminsayshi_plays_the_3d_egg_fullscreen_and_pump_retires_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");
        mode.enter_fullscreen().expect("enter fullscreen");

        mode.submit("/arminsayshi");
        mode.tui.render_now(false);
        assert!(mode.tui.has_overlay(), "the 3D egg is playing");
        mode.easter_egg
            .as_ref()
            .expect("the host owns the handle")
            .done
            .store(true, Ordering::SeqCst);
        mode.pump();
        assert!(mode.easter_egg.is_none(), "the egg was retired");
        assert!(!mode.tui.has_overlay(), "the overlay was hidden");

        mode.shutdown().expect("shutdown");
    }

    /// A header-logo click: the callback queues the logo's position and the
    /// pump replays it as the 3D pi logo — only fullscreen, where the egg
    /// can play (`header.onLogoClick`, `playPiLogo3d`); the flat tree
    /// swallows the click with no fallback.
    #[test]
    fn header_logo_click_plays_the_pi_logo_egg_in_fullscreen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.easter_egg_requests.lock().expect("queue").push((0, 0));
        mode.pump();
        assert!(mode.easter_egg.is_none(), "no egg in the flat tree");

        mode.enter_fullscreen().expect("enter fullscreen");
        mode.easter_egg_requests.lock().expect("queue").push((0, 0));
        mode.pump();
        mode.tui.render_now(false);
        assert!(mode.easter_egg.is_some(), "the logo lifts off");
        assert!(mode.tui.has_overlay());

        mode.shutdown().expect("shutdown");
    }

    /// `/name`'s argument parsing: everything after the command word,
    /// trimmed (`interactive-mode.ts:6860`).
    #[test]
    fn name_argument_strips_the_command_word_and_trims() {
        assert_eq!(name_argument("/name"), "");
        assert_eq!(name_argument("/name "), "");
        assert_eq!(name_argument("/name My Session"), "My Session");
        assert_eq!(name_argument("/name   spaced   "), "spaced");
        assert_eq!(name_argument("/other"), "");
    }

    /// `JSON.stringify` for the normalization warning: a JSON string literal.
    #[test]
    fn json_stringify_produces_a_json_string_literal() {
        assert_eq!(json_stringify("plain"), "\"plain\"");
        assert_eq!(
            json_stringify("a \"quoted\" name"),
            "\"a \\\"quoted\\\" name\""
        );
    }

    /// `/name <name>` persists the name; bare `/name` shows it; on an empty
    /// session it warns with the usage (`interactive-mode.ts:6859-6883`).
    #[test]
    fn name_command_sets_shows_and_warns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        // No name yet: the usage warning.
        mode.submit("/name");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Warning: Usage: /name <name>"), "{frame}");

        // Set one.
        mode.submit("/name Renamed session");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(
            frame.contains("Session name set: Renamed session"),
            "{frame}"
        );
        assert_eq!(
            mode.session.get_session_name().as_deref(),
            Some("Renamed session")
        );

        // Bare `/name` shows the persisted name.
        mode.submit("/name");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Session name: Renamed session"), "{frame}");

        mode.shutdown().expect("shutdown");
    }

    /// A synthetic assistant reply (`store.rs`'s test helper shape; no real
    /// data).
    fn assistant_reply(text: &str, stop_reason: pi_ai::types::StopReason) -> AgentMessage {
        use pi_ai::types::{AssistantContent, AssistantMessage, TextContent, Usage, UsageCost};
        AgentMessage::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            api: "test".into(),
            provider: "test".into(),
            model: "test-model".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: Vec::new(),
            usage: Usage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                cache_write_1h: None,
                reasoning: None,
                total_tokens: 0,
                cost: UsageCost::default(),
            },
            stop_reason,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
            duration_ms: None,
        })
    }

    /// `getLastAssistantText` (`agent-session.ts:4528-4554`): the newest
    /// assistant reply's text; a turn that did not end normally with no text
    /// is skipped (#12119).
    #[test]
    fn last_assistant_text_extracts_the_newest_reply() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        assert_eq!(mode.last_assistant_text(), None);

        mode.session
            .append_message(assistant_reply(
                "first reply",
                pi_ai::types::StopReason::Stop,
            ))
            .expect("append");
        mode.session
            .append_message(assistant_reply(
                "second reply",
                pi_ai::types::StopReason::Stop,
            ))
            .expect("append");
        assert_eq!(mode.last_assistant_text().as_deref(), Some("second reply"));

        // An aborted turn without text is skipped, not copied.
        mode.session
            .append_message(assistant_reply("", pi_ai::types::StopReason::Aborted))
            .expect("append");
        assert_eq!(mode.last_assistant_text().as_deref(), Some("second reply"));

        mode.shutdown().expect("shutdown");
    }

    /// `/copy` on an empty session shows the upstream error; the clipboard
    /// outcome itself is environment-dependent (OSC 52 headless, `xclip`, …),
    /// covered by the [`crate::clipboard`] tests.
    #[test]
    fn copy_command_on_an_empty_session_reports_no_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/copy");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("No agent messages to copy yet."), "{frame}");

        mode.shutdown().expect("shutdown");
    }

    /// `toLocaleString()`'s en-US grouping.
    #[test]
    fn to_locale_string_groups_thousands() {
        assert_eq!(to_locale_string(0), "0");
        assert_eq!(to_locale_string(999), "999");
        assert_eq!(to_locale_string(1_000), "1,000");
        assert_eq!(to_locale_string(1_234_567), "1,234,567");
        assert_eq!(to_locale_string(18_500), "18,500");
    }

    /// `renderInfo` (`interactive-mode.ts:6900-6960`) on a synthetic stats
    /// snapshot: identity, counts, the cached/uncached split, cache warming,
    /// and the cost block with the re-billed detail.
    #[test]
    fn session_info_text_renders_the_upstream_block() {
        use crate::modes::interactive::components::footer::{UsageCostBreakdownEntry, UsageTotals};
        let stats = SessionStats {
            session_id: "test-session".to_string(),
            total_messages: 4,
            user_messages: 2,
            assistant_messages: 2,
            tool_calls: 1,
            tool_results: 1,
            tokens: UsageTotals {
                input: 10_000,
                output: 500,
                cache_read: 8_000,
                cache_write: 0,
                cost: 0.012,
            },
            ..SessionStats::default()
        };
        let cache_waste = crate::session::cache_stats::CacheWasteTotals {
            missed_tokens: 3_000,
            missed_cost: 0.009,
            miss_count: 2,
        };
        let info = session_info_text(
            &stats,
            Some("Named session"),
            "idle",
            &cache_waste,
            &[UsageCostBreakdownEntry {
                key: "test/test-model".to_string(),
                cost: 0.012,
                tokens: 18_500,
            }],
            "test/test-model",
        );
        let plain = pi_tui::ansi::strip_terminal_sequences(&info);
        assert!(plain.contains("Session Info"), "{plain}");
        assert!(plain.contains("Name: Named session"), "{plain}");
        assert!(plain.contains("File: In-memory"), "{plain}");
        assert!(plain.contains("ID: test-session"), "{plain}");
        assert!(plain.contains("Total: 4"), "{plain}");
        assert!(plain.contains("Tools: 1 calls, 1 results"), "{plain}");
        // prompt = 18,000; 8,000 served from cache (44.4%).
        assert!(plain.contains("Input: 18,000"), "{plain}");
        assert!(plain.contains("Cached: 8,000 (44.4%)"), "{plain}");
        assert!(plain.contains("Uncached: 10,000"), "{plain}");
        assert!(plain.contains("Output: 500"), "{plain}");
        assert!(plain.contains("Total: 18,500"), "{plain}");
        assert!(plain.contains("Mode: idle"), "{plain}");
        assert!(
            plain.contains("Status: Inactive (cache warming unavailable)"),
            "{plain}"
        );
        // A single breakdown entry naming the selected model repeats only
        // the total; the re-billed detail rides on.
        assert!(plain.contains("Total: $0.012"), "{plain}");
        assert!(
            plain.contains("Cache Re-billed: $0.009 (3,000 tokens, 2 misses)"),
            "{plain}"
        );
    }

    /// The per-model breakdown line appears when usage names a model other
    /// than the selected one (`interactive-mode.ts:6945-6951`).
    #[test]
    fn session_info_text_breaks_out_other_models() {
        use crate::modes::interactive::components::footer::{UsageCostBreakdownEntry, UsageTotals};
        let stats = SessionStats {
            session_id: "s".to_string(),
            tokens: UsageTotals {
                input: 1_000,
                output: 0,
                cache_read: 0,
                cache_write: 0,
                cost: 0.02,
            },
            ..SessionStats::default()
        };
        let info = session_info_text(
            &stats,
            None,
            "idle",
            &crate::session::cache_stats::CacheWasteTotals::default(),
            &[
                UsageCostBreakdownEntry {
                    key: "other/virtual".to_string(),
                    cost: 0.015,
                    tokens: 700,
                },
                UsageCostBreakdownEntry {
                    key: "test/test-model".to_string(),
                    cost: 0.005,
                    tokens: 300,
                },
            ],
            "test/test-model",
        );
        let plain = pi_tui::ansi::strip_terminal_sequences(&info);
        assert!(plain.contains("Total: $0.020"), "{plain}");
        assert!(
            plain.contains("other/virtual: $0.015 (700 tokens)"),
            "{plain}"
        );
        assert!(
            plain.contains("test/test-model: $0.005 (300 tokens)"),
            "{plain}"
        );
    }

    /// `/session` on an in-memory session renders the block headlessly.
    #[test]
    fn session_command_renders_the_info_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/session");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Session Info"), "{frame}");
        assert!(frame.contains("File: In-memory"), "{frame}");
        assert!(frame.contains("Cache Warming"), "{frame}");

        mode.shutdown().expect("shutdown");
    }

    /// `keyDisplayText` (`keybinding-hints.ts:38-40`): all bound keys joined
    /// with `/`, parts capitalized; editor actions resolve through pi-tui,
    /// app actions through the app table.
    #[test]
    fn key_display_text_joins_and_capitalizes() {
        assert_eq!(
            theme::key_display_text("tui.editor.cursorLeft"),
            "Left/Ctrl+B"
        );
        assert_eq!(theme::key_display_text("tui.editor.cursorUp"), "Up");
        assert_eq!(
            theme::key_display_text(theme::app_keys::MODEL_SELECT),
            "Ctrl+L"
        );
        assert_eq!(
            theme::key_display_text(theme::app_keys::MODEL_CYCLE_BACKWARD),
            "Ctrl+Shift+P"
        );
    }

    /// `handleHotkeysCommand`'s table (`interactive-mode.ts:6999-7099`): the
    /// three sections with live bindings filled in.
    #[test]
    fn hotkeys_markdown_lists_the_upstream_sections() {
        let markdown = hotkeys_markdown();
        let plain = pi_tui::ansi::strip_terminal_sequences(&markdown);
        assert!(plain.contains("**Navigation**"), "{plain}");
        assert!(plain.contains("**Editing**"), "{plain}");
        assert!(plain.contains("**Other**"), "{plain}");
        assert!(plain.contains("| `Enter` | Send message |"), "{plain}");
        assert!(
            plain.contains("`Left/Ctrl+B` / `Right/Ctrl+F` | Move cursor / browse history"),
            "{plain}"
        );
        assert!(
            plain.contains("| `Escape` | Cancel autocomplete / abort streaming |"),
            "{plain}"
        );
        assert!(
            plain.contains("| `Ctrl+L` | Open model selector |"),
            "{plain}"
        );
        assert!(
            plain.contains("| `Ctrl+X` | Copy selection or last assistant message |"),
            "{plain}"
        );
        // The trailing literal rows carry no bindings.
        assert!(plain.contains("| `/` | Slash commands |"), "{plain}");
        assert!(
            plain.contains("| `!!` | Run bash command (excluded from context) |"),
            "{plain}"
        );
    }

    /// `/hotkeys` renders the bordered shortcut table.
    #[test]
    fn hotkeys_command_renders_the_shortcuts_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/hotkeys");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Keyboard Shortcuts"), "{frame}");
        assert!(frame.contains("Send message"), "{frame}");

        mode.shutdown().expect("shutdown");
    }

    /// `/changelog` without a shipped changelog shows the empty state
    /// (`interactive-mode.ts:6968-6973`).
    #[test]
    fn changelog_command_without_entries_shows_the_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/changelog");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("What's New"), "{frame}");
        assert!(frame.contains("No changelog entries found."), "{frame}");

        mode.shutdown().expect("shutdown");
    }

    // --------------------------------------------------------------
    // /login and /logout (slice 6c)
    // --------------------------------------------------------------

    /// `/logout` without stored credentials shows the verbatim empty state
    /// (`interactive-mode.ts:6219-6221`).
    #[test]
    fn logout_without_credentials_shows_the_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/logout");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("No stored credentials to remove"), "{frame}");
        assert!(
            // The upstream status line wraps at terminal width; match the
            // second line's fragment.
            frame.contains("variables and models.json config are unchanged"),
            "{frame}"
        );
        assert!(mode.suspended_editor.is_none(), "no selector mounted");

        mode.shutdown().expect("shutdown");
    }

    /// `/logout` with stored credentials lists them; confirming a row deletes
    /// that entry only, with the upstream status
    /// (`interactive-mode.ts:6222-6246`).
    #[test]
    fn logout_deletes_the_selected_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        // Synthetic credentials only (never a real auth.json).
        std::fs::write(
            &auth_path,
            r#"{"groq": {"type": "api_key", "key": "sk-test"}, "acme": {"type": "oauth", "access": "tok", "refresh": "r", "expires": 9000000000000}}"#,
        )
        .expect("write auth.json");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/logout");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Select provider to logout:"), "{frame}");
        assert!(frame.contains("groq"), "{frame}");
        assert!(frame.contains("acme"), "{frame}");

        // Confirming the api_key row (escape is the other exit; the pump
        // drains the selector queue like `showSelector`'s callbacks).
        mode.auth_selector_events
            .as_ref()
            .expect("selector")
            .lock()
            .expect("selector events")
            .push(AuthSelectorEvent::Select {
                provider_id: "groq".to_string(),
                auth_type: "api_key",
            });
        mode.pump();
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Removed stored API key for groq"), "{frame}");
        assert!(
            frame.contains("Environment variables and models.json config are unchanged"),
            "{frame}"
        );
        let stored = std::fs::read_to_string(&auth_path).expect("auth.json");
        assert!(stored.contains("acme"), "{stored}");
        assert!(!stored.contains("groq"), "{stored}");
        assert!(mode.suspended_editor.is_none(), "editor restored");

        mode.shutdown().expect("shutdown");
    }

    /// `/login <provider>` for an API-key provider opens the entry prompt,
    /// and the submitted key lands in auth.json with the upstream status
    /// (`interactive-mode.ts:6397-6447` + `completeProviderAuthentication`).
    #[test]
    fn login_saves_the_submitted_api_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/login groq");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Enter Groq API key"), "{frame}");
        assert!(mode.login_prompt_events.is_some(), "prompt mounted");
        assert!(
            mode.login_prompt_provider.as_ref().expect("provider").0 == "groq",
            "prompt targets groq"
        );

        mode.login_prompt_events
            .as_ref()
            .expect("prompt")
            .lock()
            .expect("prompt events")
            .push(LoginPromptEvent::Submit("sk-test-123".to_string()));
        mode.pump();
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Saved API key for groq"), "{frame}");
        assert!(frame.contains("Credentials saved to"), "{frame}");
        let stored = std::fs::read_to_string(dir.path().join("auth.json")).expect("auth.json");
        assert!(stored.contains("\"groq\""), "{stored}");
        assert!(stored.contains("sk-test-123"), "{stored}");
        assert!(mode.suspended_editor.is_none(), "editor restored");

        mode.shutdown().expect("shutdown");
    }

    /// A cancelled login prompt reopens nothing and restores the editor
    /// (upstream rejects with `Login cancelled`; the port has no back target
    /// from a direct `/login <ref>`).
    #[test]
    fn login_prompt_cancel_restores_the_editor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/login groq");
        assert!(mode.login_prompt_events.is_some(), "prompt mounted");
        mode.login_prompt_events
            .as_ref()
            .expect("prompt")
            .lock()
            .expect("prompt events")
            .push(LoginPromptEvent::Cancel);
        mode.pump();
        assert!(mode.suspended_editor.is_none(), "editor restored");
        assert!(mode.login_prompt_events.is_none(), "prompt closed");
        assert!(
            !dir.path().join("auth.json").exists() || {
                let stored =
                    std::fs::read_to_string(dir.path().join("auth.json")).expect("auth.json");
                stored.trim() == "{}" || !stored.contains("groq")
            },
            "no credential written"
        );

        mode.shutdown().expect("shutdown");
    }

    /// `/login` without a reference opens the upstream auth-type menu
    /// (`interactive-mode.ts:6087-6155`).
    #[test]
    fn login_without_reference_opens_the_auth_type_menu() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/login");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Select authentication method:"), "{frame}");
        assert!(frame.contains("Sign in with an account"), "{frame}");
        assert!(frame.contains("Sign in with an API key"), "{frame}");
        // The Radius option rides last (`interactive-mode.ts:6090-6093`).
        assert!(frame.contains("Sign in with radius"), "{frame}");

        mode.shutdown().expect("shutdown");
    }

    /// `/login` for an OAuth provider reports the unimplemented flow through
    /// the upstream error shape (the slice 6c deviation: the per-provider
    /// OAuth protocol machines in `packages/ai/src/auth/oauth/` are not
    /// ported).
    #[test]
    fn login_oauth_reports_the_unimplemented_flow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        mode.submit("/login openai-codex");
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Failed to login to"), "{frame}");
        assert!(
            frame.contains("OAuth login flows are not implemented"),
            "{frame}"
        );

        mode.shutdown().expect("shutdown");
    }
}
