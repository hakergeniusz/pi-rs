//! The interactive mode orchestrator: the chat TUI over the agent loop.
//!
//! Ground truth: `modes/interactive/interactive-mode.ts`. This port covers the
//! regular (main-screen) chat loop — the startup header, user and assistant
//! messages, tool execution rows, the working status embedded in the editor's
//! top border, the footer, the `/`-commands this phase wires, and the `!` bash
//! mode — one agent run at a time on a worker thread. Deferred to later P6
//! parts: fullscreen mode, autocomplete dialogs (model/session/settings
//! selectors), extensions, compaction, follow-up queues, and the
//! mouse-driven toggles.

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
use pi_tui::terminal::{CrosstermTerminal, key_to_terminal_data};
use pi_tui::tui::FocusTarget;
use pi_tui::tui::{Tui, TuiStopOptions};

use crate::modes::print_mode::PrintRuntime;
use crate::session::store::{NewSessionOptions, SessionManager};
use crate::tools::index::BuiltinToolExecutor;

use super::components::assistant_message::AssistantMessageComponent;
use super::components::footer::{FooterComponent, FooterData, UsageTotals};
use super::components::header::{BuiltInHeader, pi_logo_lines};
use super::components::status_indicator::{IdleStatus, StatusIndicator};
use super::components::tool_execution::{ToolExecutionComponent, ToolResultView};
use super::components::user_message::UserMessageComponent;
use super::theme;

/// Options the CLI host passes in (`InteractiveModeOptions`).
pub struct InteractiveOptions {
    pub version: String,
    /// A `--theme` override; the settings getter lands with the settings work.
    pub theme_name: Option<String>,
}

impl Default for InteractiveOptions {
    fn default() -> Self {
        InteractiveOptions {
            version: env!("CARGO_PKG_VERSION").to_string(),
            theme_name: None,
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

/// Tool-row bookkeeping: the row's index inside the chat container.
struct PendingTool {
    chat_index: usize,
}

/// Tree child indices.
const HEADER_INDEX: usize = 0;
const CHAT_INDEX: usize = 1;
const EDITOR_INDEX: usize = 3;
const FOOTER_INDEX: usize = 4;

/// The interactive mode (`InteractiveMode`, reduced to the chat core).
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
    /// ctrl+c twice within 500ms exits (`handleCtrlC`).
    last_sigint_time: Option<Instant>,
    /// Spinner-frame pacing, carried across `pump` calls.
    last_spinner_tick: Instant,
    /// ctrl+o state: tool output expansion and the header hints.
    tool_output_expanded: bool,
    hide_thinking_block: bool,
    exiting: bool,
}

impl InteractiveMode {
    fn new(
        runtime: &mut PrintRuntime,
        options: &InteractiveOptions,
        terminal: Box<dyn pi_tui::terminal::Terminal>,
    ) -> Self {
        theme::init_theme(options.theme_name.as_deref());
        let editor_events = Arc::new(Mutex::new(Vec::new()));
        let status_handle: Arc<Mutex<Option<StatusIndicator>>> = Arc::new(Mutex::new(None));
        let editor_host = ChatEditor::new(Arc::clone(&editor_events), Arc::clone(&status_handle));

        let mut tui = Tui::new(terminal);
        tui.set_clear_on_shrink(true);
        tui.add_child(Box::new(Self::build_header(
            &options.version,
            &runtime.model.id,
        )));
        tui.add_child(Box::new(pi_tui::component::Container::new()));
        tui.add_child(Box::new(IdleStatus));
        tui.add_child(Box::new(editor_host));
        tui.add_child(Box::new(FooterComponent::new(Self::footer_data(runtime))));
        tui.set_focus(Some(FocusTarget::Root(EDITOR_INDEX)));

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
        }
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
            self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new()));
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
                AgentMessage::User(_) => {
                    // User messages persist at submit time.
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
        if self.agent_run.is_some() {
            // Upstream steers/queues; the queue UI lands with the queues work.
            self.show_status("The agent is still responding — press escape to interrupt first.");
            return;
        }
        self.start_agent_run(&text);
    }

    /// `/model [pattern]`: without a pattern show the current model; with one,
    /// resolve and switch (`handleModelCommand`).
    fn handle_model_command(&mut self, pattern: &str) {
        if pattern.is_empty() {
            self.show_status(&format!(
                "Current model: {}/{}",
                self.model.provider, self.model.id
            ));
            return;
        }
        let parsed = crate::modes::print_mode::parse_model_pattern(pattern);
        match crate::modes::print_mode::resolve_model(Some(&parsed), None, None) {
            Ok(model) => {
                if model.api != self.model.api {
                    let Some(stream_fn) = pi_ai::providers::stream_fn_for(&model.api) else {
                        self.show_error(&format!(
                            "No provider adapter for api \"{}\" (model {}/{}).",
                            model.api, model.provider, model.id
                        ));
                        return;
                    };
                    self.stream_fn =
                        Arc::new(crate::modes::print_mode::ProviderStreamFn(stream_fn));
                }
                let changed = model.provider != self.model.provider || model.id != self.model.id;
                self.model = model;
                if changed {
                    let _ = self
                        .session
                        .append_model_change(&self.model.provider, &self.model.id);
                }
                self.show_status(&format!(
                    "Switched to {}/{}",
                    self.model.provider, self.model.id
                ));
                self.refresh_footer();
                self.tui.request_render();
            }
            Err(error) => self.show_error(&error),
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
            self.show_status("Usage: /theme <system|dark|light>");
            return;
        }
        match theme::set_theme(name) {
            Ok(()) => {
                self.show_status(&format!("Theme: {name}"));
                self.tui.request_render();
            }
            Err(error) => self.show_error(&error),
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
        let handle = std::thread::spawn(move || {
            let mut queues = Queues::new();
            let mut sink = ChannelSink { tx: tx.clone() };
            let _messages = crate::runtime::block_on(pi_agent::run_agent_loop(
                vec![user_message],
                context,
                executor.as_ref(),
                &config,
                &mut queues,
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
        self.tui
            .start()
            .map_err(|error| format!("failed to start the terminal: {error}"))?;
        self.tui.request_render();
        loop {
            self.pump();
            if self.exiting {
                break;
            }

            // Terminal input.
            if !crossterm::event::poll(Duration::from_millis(25))
                .map_err(|error| format!("input poll failed: {error}"))?
            {
                self.tui.render_now(false);
                continue;
            }
            let event = match crossterm::event::read() {
                Ok(event) => event,
                Err(error) => return Err(format!("input read failed: {error}")),
            };
            let crossterm::event::Event::Key(key) = event else {
                continue;
            };
            if key.kind == crossterm::event::KeyEventKind::Release {
                continue;
            }
            let Some(data) = key_to_terminal_data(key) else {
                continue;
            };
            self.tui.handle_input(&data);
            self.tui.render_now(false);
        }

        self.shutdown()
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
                if let Some(run) = &self.agent_run {
                    run.abort.abort();
                }
            }
            AppAction::CycleModel(_direction) => {
                // Model cycling over the catalog lands with the model
                // selector (P6 part 2).
            }
            AppAction::ToggleThinking => {
                self.hide_thinking_block = !self.hide_thinking_block;
                self.tui.request_render();
            }
            AppAction::ToolsExpand => {
                self.tool_output_expanded = !self.tool_output_expanded;
                let expanded = self.tool_output_expanded;
                let len = self.chat_len();
                for index in 0..len {
                    if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(index) {
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
        let mut runtime = PrintRuntime {
            model,
            thinking_level: ThinkingLevel::default(),
            api_key: Some("test".to_string()),
            access_token: None,
            stream_fn: stream,
            executor: ToolRegistry::read_only().executor(dir),
            system_prompt: "test".to_string(),
            session: SessionManager::in_memory(dir, &NewSessionOptions::default(), Vec::new())
                .expect("session"),
        };
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 100,
            rows: 40,
        });
        let capture = terminal.capture();
        let mode = InteractiveMode::new(
            &mut runtime,
            &InteractiveOptions::default(),
            Box::new(terminal),
        );
        (mode, capture)
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
