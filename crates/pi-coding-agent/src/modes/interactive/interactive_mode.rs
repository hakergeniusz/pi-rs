//! The interactive mode orchestrator: the chat TUI over the agent loop.
//!
//! Ground truth: `modes/interactive/interactive-mode.ts`. This port covers the
//! regular (main-screen) chat loop — the startup header, user and assistant
//! messages, tool execution rows, the working status embedded in the editor's
//! top border, the footer, the `/`-commands this phase wires, the `!` bash
//! mode, and the editor autocomplete (slash commands, `@` files, paths).
//! Deferred to later P6 parts: fullscreen mode, selector dialogs
//! (model/session/settings), extensions, compaction, follow-up queues, and
//! the mouse-driven toggles.

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_agent::agent_loop::{AgentLoopConfig, DefaultConvertToLlm};
use pi_agent::agent_message::AgentMessage;
use pi_agent::events::{AgentEvent, AgentEventSink};
use pi_agent::queues::Queues;
use pi_agent::transcript::tool_declaration;
use pi_agent::types::{AbortSignal, AgentContext, ThinkingLevel, ToolExecutionMode, ToolExecutor};
use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::editor::{Editor, EditorOptions};
use pi_tui::components::text::Text;
use pi_tui::mouse::{MouseEvent, MouseEventResult};
use pi_tui::stdin_buffer::{StdinBuffer, StdinBufferOptions, StdinEvent};
use pi_tui::terminal::CrosstermTerminal;
use pi_tui::terminal_colors::TerminalColorScheme;
use pi_tui::theme::ThemeAppearance;
use pi_tui::tui::FocusTarget;
use pi_tui::tui::{TerminalColors, Tui, TuiStopOptions};

use crate::modes::print_mode::PrintRuntime;
use crate::session::store::{NewSessionOptions, SessionManager};
use crate::tools::index::BuiltinToolExecutor;

use super::autocomplete;
use super::components::assistant_message::AssistantMessageComponent;
use super::components::footer::{FooterComponent, FooterData, UsageTotals};
use super::components::header::{BuiltInHeader, pi_logo_lines};
use super::components::model_selector::{ModelSelectorComponent, ModelSelectorEvent};
use super::components::status_indicator::{IdleStatus, StatusIndicator};
use super::components::tool_execution::{ToolExecutionComponent, ToolResultView};
use super::components::user_message::UserMessageComponent;
use super::theme;

/// Options the CLI host passes in (`InteractiveModeOptions`).
pub struct InteractiveOptions {
    pub version: String,
    /// A `--theme` override; the settings getter lands with the settings work.
    pub theme_name: Option<String>,
    /// The `auth.json` path for the model-availability check. `None` uses the
    /// real agent dir; tests inject a temp path to stay hermetic.
    pub auth_path: Option<std::path::PathBuf>,
}

impl Default for InteractiveOptions {
    fn default() -> Self {
        InteractiveOptions {
            version: env!("CARGO_PKG_VERSION").to_string(),
            theme_name: None,
            auth_path: None,
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

/// Tree child indices. The pending-messages container sits between the chat
/// and the status row (`interactive-mode.ts` `pendingMessagesContainer`).
const HEADER_INDEX: usize = 0;
const CHAT_INDEX: usize = 1;
const PENDING_INDEX: usize = 2;
const EDITOR_INDEX: usize = 4;
const FOOTER_INDEX: usize = 5;

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
    executor: Arc<BuiltinToolExecutor>,
    system_prompt: String,

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
    /// The editor, parked while the selector occupies its tree slot.
    suspended_editor: Option<Box<dyn Component>>,
    /// Where provider credentials live (`auth.json`).
    auth_path: std::path::PathBuf,
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
        let provider = autocomplete::CombinedAutocompleteProvider::new(
            Self::autocomplete_commands(),
            runtime.session.get_cwd().to_path_buf(),
            crate::tools::bash::which("fd"),
        );
        editor_host
            .editor
            .set_autocomplete_provider(Some(Arc::new(provider)));

        let mut tui = Tui::new(terminal);
        tui.set_clear_on_shrink(true);
        tui.add_child(Box::new(Self::build_header(
            &options.version,
            &runtime.model.id,
        )));
        tui.add_child(Box::new(pi_tui::component::Container::new()));
        tui.add_child(Box::new(pi_tui::component::Container::new()));
        tui.add_child(Box::new(IdleStatus));
        tui.add_child(Box::new(editor_host));
        tui.add_child(Box::new(FooterComponent::new(Self::footer_data(runtime))));
        tui.set_focus(Some(FocusTarget::Root(EDITOR_INDEX)));

        // Mode-2031 reports reach the controller through a listener the TUI
        // dispatches (`bindTerminalColorSchemeListener`); the closure cannot
        // borrow the mode, so it queues.
        let scheme_queue = Arc::clone(&theme_scheme_reports);
        tui.add_color_scheme_listener(Arc::new(move |scheme| {
            if let Ok(mut pending) = scheme_queue.lock() {
                pending.push(scheme);
            }
        }));
        // The initial color query (`applyFromSettings` →
        // `requestTerminalColors`); late replies ride the sink.
        let theme_query_start = Some(Instant::now());
        let late_queue = Arc::clone(&theme_late_reports);
        let sink = Box::new(move |colors: TerminalColors| {
            if let Ok(mut pending) = late_queue.lock() {
                pending.push(colors);
            }
        });
        let _ = tui.query_terminal_colors(100, Some(sink));
        tui.set_color_scheme_notifications(theme_auto_sync);

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
            pending_tools: std::collections::HashMap::new(),
            streaming_index: None,
            agent_run: None,
            last_sigint_time: None,
            last_spinner_tick: Instant::now(),
            tool_output_expanded: false,
            hide_thinking_block: false,
            exiting: false,
            model_selector_events: None,
            suspended_editor: None,
            auth_path: options
                .auth_path
                .clone()
                .unwrap_or_else(crate::config::get_auth_path),
            queues: Arc::new(Mutex::new(Queues::new())),
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

    /// The `/`-commands offered by autocomplete (`createBaseAutocompleteProvider`,
    /// `interactive-mode.ts:743-853`). Only the commands this port implements
    /// are listed — advertising upstream's unimplemented builtins would make
    /// the picker lie. Argument completions: `/model` fuzzy-searches the
    /// whole catalog (upstream: the scoped-models subset or the runtime
    /// snapshot; the scoped-models setting lands later), `/thinking` the
    /// valid levels.
    fn autocomplete_commands() -> Vec<autocomplete::SlashCommand> {
        let commands = vec![
            autocomplete::SlashCommand::new("model", "Select model (opens selector UI)")
                .with_argument_hint("<provider/model>")
                .with_argument_completions(Arc::new(|prefix: &str| {
                    let catalog = pi_ai::catalog::catalog();
                    let mut models: Vec<(String, String, String)> = Vec::new(); // (id, provider, name)
                    for provider in catalog.providers() {
                        for model in catalog.models(provider) {
                            models.push((
                                model.id.clone(),
                                provider.to_string(),
                                model.name.clone(),
                            ));
                        }
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
        self.downcast_tree::<pi_tui::component::Container>(CHAT_INDEX)
            .expect("chat container")
    }

    fn downcast_tree<T: 'static>(&mut self, index: usize) -> Option<&mut T> {
        self.tui.child_mut(index)?.as_any_mut()?.downcast_mut::<T>()
    }

    fn downcast_chat<T: 'static>(&mut self, index: usize) -> Option<&mut T> {
        self.downcast_tree::<pi_tui::component::Container>(CHAT_INDEX)
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
        self.downcast_tree::<ChatEditor>(EDITOR_INDEX)
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
            | AgentEvent::ToolExecutionEnd { .. } => {
                // The final tool result renders with its message end.
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
        let Some(footer) = self.downcast_tree::<FooterComponent>(FOOTER_INDEX) else {
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
        if let Some(rest) = text.strip_prefix("/model") {
            self.handle_model_command(rest.trim());
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

        let model = self.model.clone();
        let api_key = self.api_key.clone();
        let access_token = self.access_token.clone();
        let thinking_level = self.thinking_level;
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
            .downcast_tree::<pi_tui::component::Container>(PENDING_INDEX)
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
        match crate::modes::print_mode::resolve_model(Some(&parsed), None, None) {
            Ok(model) => match self.apply_model(model) {
                Ok(()) => self.show_status(&format!("Model: {}", self.model.id)),
                Err(error) => self.show_error(&error),
            },
            Err(_) => self.open_model_selector(Some(pattern)),
        }
    }

    /// Swap the active model, including the provider adapter when the api
    /// changes (`handleModelCommand` and `session.cycleModel` share the path).
    fn apply_model(&mut self, model: pi_ai::types::Model) -> Result<(), String> {
        if model.api != self.model.api {
            let Some(stream_fn) = pi_ai::providers::stream_fn_for(&model.api) else {
                return Err(format!(
                    "No provider adapter for api \"{}\" (model {}/{}).",
                    model.api, model.provider, model.id
                ));
            };
            self.stream_fn = Arc::new(crate::modes::print_mode::ProviderStreamFn(stream_fn));
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
    fn available_models(&self) -> Vec<pi_ai::types::Model> {
        let auth_path = &self.auth_path;
        let catalog = pi_ai::catalog::catalog();
        let mut models = Vec::new();
        for provider in catalog.providers() {
            let flag_key = if provider == self.model.provider {
                self.api_key.clone()
            } else {
                None
            };
            if crate::modes::print_mode::resolve_wire_credential(
                provider,
                flag_key.as_deref(),
                auth_path,
            )
            .is_some()
            {
                models.extend(catalog.models(provider).into_iter().cloned());
            }
        }
        models
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
        let editor = self.tui.replace_child(EDITOR_INDEX, Box::new(selector));
        self.suspended_editor = Some(editor);
        self.model_selector_events = Some(events);
        self.tui.set_focus(Some(FocusTarget::Root(EDITOR_INDEX)));
        self.tui.request_render();
    }

    /// Put the editor back (`showSelector`'s `done`).
    fn close_model_selector(&mut self) {
        if self.suspended_editor.is_none() {
            return;
        }
        let editor = self.suspended_editor.take().expect("suspended editor");
        let _ = self.tui.replace_child(EDITOR_INDEX, editor);
        self.model_selector_events = None;
        self.tui.set_focus(Some(FocusTarget::Root(EDITOR_INDEX)));
        self.tui.request_render();
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
                    text,
                    is_error: exit_code.is_none_or(|code| code != 0),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                },
                false,
            );
        }
        let _ = self.session.append_message(AgentMessage::BashExecution(
            pi_agent::agent_message::BashExecutionMessage {
                command: command.to_string(),
                output: String::new(),
                exit_code,
                cancelled: false,
                truncated: false,
                full_output_path: None,
                exclude_from_context: Some(excluded),
                timestamp: now_millis(),
            },
        ));
        self.tui.request_render();
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
            prepare_request: None,
            prepare_next_turn: None,
            transform_context: None,
            convert_to_llm: Arc::new(DefaultConvertToLlm),
        };

        let (tx, rx) = std::sync::mpsc::channel::<LoopEvent>();
        let signal = Arc::new(AbortSignal::new());
        let executor = Arc::clone(&self.executor);
        let stream_fn = Arc::clone(&self.stream_fn);
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

    /// One loop iteration without terminal input: editor events, agent
    /// events, spinner frames. `run` calls this between input polls; the
    /// scripted tests drive it directly with a capture terminal.
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

        // Selector events, drained through the Arc the component publishes to
        // (`showSelector`'s onSelect/onCancel callbacks).
        if self.model_selector_events.is_some() {
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
            if let Some(run) = self.agent_run.take()
                && let Some(handle) = run.handle
            {
                let _ = handle.join();
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
                if let Some(header) = self.downcast_tree::<BuiltInHeader>(HEADER_INDEX) {
                    header.set_expanded(expanded);
                }
                self.tui.request_render();
            }
        }
    }

    /// `shutdown` (`interactive-mode.ts:4475`): restore the terminal, print
    /// the resume command (`formatResumeCommand`).
    fn shutdown(&mut self) -> Result<(), String> {
        let session_id = self.session.get_session_id().to_string();
        let uses_default_dir = self.session.uses_default_session_dir();
        let session_dir = self.session.get_session_dir().to_string_lossy().to_string();
        self.tui
            .stop(TuiStopOptions::default())
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
    Ok(0)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
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
            .downcast_tree::<ModelSelectorComponent>(EDITOR_INDEX)
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
            .downcast_tree::<ChatEditor>(EDITOR_INDEX)
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
            .downcast_tree::<pi_tui::component::Container>(PENDING_INDEX)
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
}
