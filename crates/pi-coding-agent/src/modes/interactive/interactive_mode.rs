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
    format_http_idle_timeout_ms,
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
#[cfg(feature = "ext-js")]
use super::components::component_proxy::ComponentProxy;
use super::components::easter_egg_3d::{EasterEgg3d, EasterEggHandle, play_easter_egg_3d};
#[cfg(feature = "ext-js")]
use super::components::extension_custom::ExtensionCustomComponent;
use super::components::extension_editor::{ExtensionEditorComponent, ExtensionEditorEvent};
#[cfg(feature = "ext-js")]
use super::components::extension_input::{ExtensionInputComponent, ExtensionInputEvent};
use super::components::extension_selector::{ExtensionSelectorComponent, ExtensionSelectorEvent};
// The bridge's request payloads (E3 slice 6a); the rest of the file qualifies.
use super::components::footer::{FooterComponent, FooterData, UsageTotals};
use super::components::header::{BuiltInHeader, pi_logo_lines};
use super::components::model_selector::{ModelSelectorComponent, ModelSelectorEvent};
use super::components::scoped_models_selector::{
    ModelsConfig, RefreshStatusKind, ScopedModelsSelectorComponent, ScopedModelsSelectorEvent,
};
use super::components::session_selector::{SessionSelectorComponent, SessionSelectorEvent};
use super::components::settings_selector::{
    SettingsSelectorComponent, SettingsSelectorConfig, SettingsSelectorEvent,
};
use super::components::status_indicator::{IdleStatus, StatusIndicator, StatusKind};
use super::components::tool_execution::{ToolExecutionComponent, ToolResultView};
use super::components::tree_selector::{TreeSelectorComponent, TreeSelectorEvent};
use super::components::trust_selector::{TrustSelectorComponent, TrustSelectorEvent};
use super::components::user_message::UserMessageComponent;
use super::components::user_message_selector::{
    UserMessageItem, UserMessageSelectorComponent, UserMessageSelectorEvent,
};
use super::model_catalog_refresh::refresh_model_catalogs;
use super::program_status::{BlockedStatus, ProgramStatusReporter};
use super::theme;
use crate::cli::args::DiagnosticKind;
use crate::model_resolver::{
    ModelScopeDiagnosticCode, ScopedModel, models_are_equal, resolve_model_scope_from_models,
};
use crate::oauth::{self, AuthSelectorProvider};
#[cfg(feature = "ext-js")]
use serde_json::Value;

/// Where a cancelled login returns to (`startProviderLogin`'s `onBack`,
/// `interactive-mode.ts:6077-6085`).
#[derive(Debug, Clone)]
enum LoginBackStep {
    /// `showLoginAuthTypeSelector(providerOptions)` again.
    AuthType(Option<Vec<AuthSelectorProvider>>),
    /// `showLoginProviderSelector(authType)` again.
    Provider(Option<&'static str>),
}

/// The pending `/import` dialog: which question the mounted selector is
/// asking and what to do with the answer (`handleImportCommand`,
/// `interactive-mode.ts:6755-6803`).
#[derive(Debug, Clone)]
enum ImportDialog {
    /// `showExtensionConfirm("Import session", …)` — the Yes/No gate.
    Confirm { input_path: String },
    /// `promptForMissingSessionCwd` — continue/fork/cancel on the copied
    /// session's stale stored cwd.
    MissingSessionCwd {
        input_path: String,
        issue: crate::session::cwd::SessionCwdIssue,
    },
}

/// What `importFromJsonl` reports (`agent-session-runtime.ts:356-357`'s
/// throws): the file-not-found error, the stale-cwd gate, and everything
/// else that rides `handleFatalRuntimeError`.
enum ImportFromJsonlError {
    /// `SessionImportFileNotFoundError` — carries its full message.
    FileNotFound(String),
    /// `MissingSessionCwdError` — the issue drives the continue/fork prompt.
    MissingSessionCwd(crate::session::cwd::SessionCwdIssue),
    /// Any other failure, rendered through the fatal-error message.
    Other(String),
}

/// One dialog on the extension stack (E3 slice 6b): which `ctx.ui` method
/// it answers, its component's event mailbox, the countdown deadline the
/// pump ticks (`opts.timeout`, `extension-selector.ts:63-72` — upstream
/// arms the countdown inside the component; the port keeps the clock in
/// the mode's pump), and the component itself: `None` while it is the
/// editor slot's occupant (visible), `Some` while parked behind a dialog
/// pushed later. Upstream keeps every dialog in `extensionDialogs`
/// (`interactive-mode.ts:584`) and `showTopExtensionDialog` decides what
/// the slot shows.
#[cfg(feature = "ext-js")]
struct ExtensionUiDialogEntry {
    id: String,
    kind: ExtensionUiKind,
    events: ExtensionUiDialogEvents,
    deadline: Option<Instant>,
    /// The seconds the countdown last rendered, so the pump only
    /// re-renders on a real tick.
    last_seconds: Option<u32>,
    /// The engine-side handle of a custom component — disposed on close
    /// (upstream's `disposeComponent`, `interactive-mode.ts:3104-3109`).
    handle: Option<u32>,
    /// The dialog's component while parked; `None` means visible.
    component: Option<Box<dyn Component>>,
    /// What the dialog blocks the program status on while it shows —
    /// `pushExtensionDialog`'s `blocked` (`interactive-mode.ts:2833-2837`),
    /// reported through `showTopExtensionDialog` (`:2853`).
    blocked: Option<BlockedStatus>,
}

/// The `ctx.ui` dialog methods the interactive bridge mounts
/// (`showExtensionSelector`/`showExtensionConfirm`/`showExtensionInput`/
/// `showExtensionEditor`/`showExtensionCustom`).
#[cfg(feature = "ext-js")]
#[derive(Debug, Clone, Copy, PartialEq)]
enum ExtensionUiKind {
    Select,
    Confirm,
    Input,
    Editor,
    Custom,
}

/// One extension overlay (`extensionOverlays`,
/// `interactive-mode.ts:590-591`): the crossing id the handle adapter is
/// bound to, the mounted overlay in the TUI, the adopted component's engine
/// handle that `disposeComponent` drops, and the numbered program-status
/// source a blocked overlay holds (`interactive-mode.ts:3094-3096`,
/// removed at `removeOverlay` `:3113-3120`). Insertion order is opening
/// order; reset removes newest-first (`:2600-2602`).
#[cfg(feature = "ext-js")]
struct ExtensionOverlayEntry {
    id: String,
    overlay: pi_tui::tui::OverlayId,
    component_handle: Option<u32>,
    blocked_source: Option<String>,
}

/// The mounted dialog's event mailbox, tagged by component.
#[cfg(feature = "ext-js")]
enum ExtensionUiDialogEvents {
    Selector(Arc<Mutex<Vec<ExtensionSelectorEvent>>>),
    Input(Arc<Mutex<Vec<ExtensionInputEvent>>>),
    Editor(Arc<Mutex<Vec<ExtensionEditorEvent>>>),
    /// A custom component interacts through the runner crossings (the
    /// wrapper's `handle_input`), not a mailbox.
    Custom,
}

/// The drained dialog event, normalized across the component types.
#[cfg(feature = "ext-js")]
enum ExtensionUiDialogEvent {
    /// A picked option (selector rows).
    Selected(String),
    /// A submitted value (input/editor lines).
    Submitted(String),
    /// Escape (or the countdown's expiry).
    Cancelled,
}

/// `copyFileSync(source, destination, COPYFILE_EXCL)` (`node:fs`): copy
/// contents and mode bits, failing when the destination already exists.
fn copy_file_exclusive(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    let mut reader = std::fs::File::open(source)?;
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    std::io::copy(&mut reader, &mut writer)?;
    let permissions = std::fs::metadata(source)?.permissions();
    std::fs::set_permissions(destination, permissions)?;
    Ok(())
}

/// One running extension-oauth login flow (`loginProvider`'s `"oauth"` arm
/// through the engine bridge, slice 6j): the JS `login` runs in an engine
/// session, its notifications surface as status lines, and its asks mount
/// the prompt/select dialogs.
#[cfg(feature = "ext-js")]
struct ExtensionLoginFlow {
    /// The engine session driving the stashed `login`.
    session: u32,
    /// The provider being configured (the storage key).
    provider_id: String,
    /// The display name for the status/error lines.
    provider_name: String,
    /// The pending ask — `(token, payload)` — when a prompt suspended the
    /// flow; the mounted dialog's answer resolves it.
    pending_prompt: Option<(u64, serde_json::Value)>,
    /// Where a cancelled login returns to (`onBack`).
    back: Option<LoginBackStep>,
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
    /// The CLI `--extension` paths, carried for `/reload`'s re-discovery
    /// (`resourceLoader.reload()` re-resolves them with the reloaded
    /// settings, `resource-loader.ts:557-560`). Tests leave them empty.
    pub extension_cli_paths: Vec<String>,
    /// The `--no-extensions` flag — CLI extensions still load (`/reload`
    /// re-applies the same split).
    pub no_extensions: bool,
    /// The interactive `ctx.ui` bridge (E3 slice 6a): the sink
    /// `extensions::load` bound and this mode drains. `None` keeps the
    /// headless no-op matrix (tests without the bridge).
    #[cfg(feature = "ext-js")]
    pub extension_ui:
        Option<std::sync::Arc<crate::modes::interactive::extension_ui::ExtensionUiBridge>>,
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
            extension_cli_paths: Vec::new(),
            no_extensions: false,
            #[cfg(feature = "ext-js")]
            extension_ui: None,
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
    /// shift+tab (`app.thinking.cycle`): cycle the thinking level.
    CycleThinking,
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
    /// The bash-mode flag flipped (`interactive-mode.ts:3249-3252`); the
    /// mode repaints the editor border.
    BashModeChanged,
    /// An extension shortcut chord matched (`custom-editor.ts:89-92` —
    /// the editor checks extension shortcuts first); the payload is the
    /// resolved key. Feature-gated so the pump's gated match arm keeps the
    /// match exhaustive without `ext-js`.
    #[cfg(feature = "ext-js")]
    ExtensionShortcut(String),
}

/// The editor host: wraps the editor, intercepts app keybindings, embeds the
/// working status in the top border, and publishes submissions/actions
/// through a shared queue (the tree cannot be downcast generically, so hosts
/// publish outward — the probe pattern from the TUI tests). This type itself
/// opts into downcasting with [`Component::as_any_mut`].
/// One extension widget (`extensionWidgetsAbove`/`...Below` values, E3
/// slice 7 — `setExtensionWidget`, `interactive-mode.ts:2538-2578`).
#[cfg(feature = "ext-js")]
#[derive(Clone)]
enum ExtensionWidget {
    /// String lines the host rebuilds as `Text(line, 1, 0)` rows.
    Lines(Vec<String>),
    /// A factory's adopted component; the registry owns the handle.
    Adopted { handle: u32, has_input: bool },
}

/// The custom footer/header swap state (`customFooter`/`customHeader`, E3
/// slice 7): the adopted handle the registry owns and the displaced
/// built-in component, restored on `undefined`/reset.
#[cfg(feature = "ext-js")]
struct ExtensionChrome {
    handle: u32,
    displaced: Box<dyn pi_tui::component::Component>,
}

/// The mode-state snapshot the synchronous ui ops read (E3 slice 7):
/// `footerData`/`editorText`/`toolsExpanded`. The pump refreshes it each
/// frame; the runner's closures read it under the mutex. E3 slice 9 adds
/// the editor chrome the editor-component wire reads.
#[cfg(feature = "ext-js")]
#[derive(Default)]
struct UiSnapshotState {
    footer: serde_json::Value,
    editor_text: String,
    tools_expanded: bool,
    editor_padding_x: usize,
    editor_max_visible: usize,
    editor_border_color: String,
    app_actions: Vec<String>,
}

/// `InteractiveMode.MAX_WIDGET_LINES` (`interactive-mode.ts:2626`): the
/// total widget lines one `setWidget` renders.
#[cfg(feature = "ext-js")]
const MAX_WIDGET_LINES: usize = 10;

/// One registered autocomplete wrapper (E3 slice 8): the engine-side
/// chain index (the order `addAutocompleteProvider` registered) and the
/// `triggerCharacters` the wrapper declared (`setupAutocompleteProvider`'s
/// union, `interactive-mode.ts:855-864`).
#[cfg(feature = "ext-js")]
struct AutocompleteWrapperRecord {
    index: u32,
    trigger_characters: Vec<char>,
}

/// The working-status row a live extension editor gets (E3 slice 9):
/// upstream's `setEditorWorkingStatusIndicator` embeds only into editors
/// that duck-type `embedWorkingStatus` (`interactive-mode.ts:232-240`) —
/// a custom component never does, so the indicator renders in
/// `statusContainer` instead (`:2445-2455`). The port's decorator belongs
/// to the built-in editor, so the delegate mounts this row in the status
/// host; it reads the same shared indicator slot each frame.
#[cfg(feature = "ext-js")]
struct ExtensionEditorStatusRow {
    status: Arc<Mutex<Option<StatusIndicator>>>,
}

#[cfg(feature = "ext-js")]
impl pi_tui::component::Component for ExtensionEditorStatusRow {
    fn render(&self, width: usize) -> Vec<String> {
        let guard = self.status.lock().expect("status");
        match guard.as_ref() {
            Some(indicator) => indicator.render(width),
            None => Vec::new(),
        }
    }
}

/// The active extension editor component, seen from the mode (E3 slice 9):
/// the adopted handle plus the state the synchronous reads serve. The
/// engine-was-busy (defer) semantics follow the terminal-input fold — keys
/// and text writes stash in pending queues the pump flushes each frame, so
/// a busy engine can never wedge the input path (the 6b deadlock class).
/// Deviations from upstream's in-process editor: the swap-time text copy
/// reads the pump-refreshed snapshot (≤ 1 frame stale), and the working
/// status never embeds into the component (`isEmbeddedStatus`,
/// `interactive-mode.ts:232-240`) — it renders in the status host row like
/// upstream's non-embedded branch.
#[cfg(feature = "ext-js")]
struct ExtensionEditorLive {
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    handle: u32,
    /// Last known text (`getExpandedText?.() ?? getText()`): the
    /// pump-refreshed mirror the synchronous reads serve.
    text: String,
    /// Key chunks deferred while the engine was busy.
    pending_input: Vec<String>,
    /// Text write deferred while the engine was busy.
    pending_set_text: Option<String>,
}

#[cfg(feature = "ext-js")]
impl ExtensionEditorLive {
    fn new(
        runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
        handle: u32,
        text: String,
    ) -> Self {
        Self {
            runner,
            handle,
            text,
            pending_input: Vec::new(),
            pending_set_text: None,
        }
    }

    /// Renders the component (`Component.render`): a blocking lock matches
    /// the widget proxies' precedent — renders run on the pump thread,
    /// which holds no runner lock of its own.
    fn render(&self, width: usize) -> Vec<String> {
        let runner = self.runner.lock().expect("extension runner mutex poisoned");
        match runner.component_render(self.handle, width) {
            Ok(lines) => lines,
            // Upstream lets a component render throw take the frame down;
            // the port renders the failure in place instead.
            Err(error) => vec![theme::theme().fg(
                pi_tui::theme::ThemeColor::Error,
                &format!("editor component render failed: {error}"),
            )],
        }
    }

    /// Feeds one key chunk (`handleInput`): a busy engine defers the chunk
    /// and consumes it — the pump redelivers it next frame, upstream
    /// equivalent to the single-threaded engine finishing first. `false` =
    /// forward (an error behaves like a non-consuming component).
    fn handle_input(&mut self, data: &str) -> bool {
        let Ok(mut runner) = self.runner.try_lock() else {
            self.pending_input.push(data.to_string());
            return true;
        };
        runner
            .component_handle_input(self.handle, data)
            .unwrap_or(false)
    }

    /// Refreshes and returns the text mirror (`getExpandedText?.() ??
    /// getText()`): a busy engine keeps the previous mirror.
    fn editor_text(&mut self) -> String {
        let Ok(runner) = self.runner.try_lock() else {
            return self.text.clone();
        };
        if let Ok(text) = runner.component_editor_text(self.handle) {
            self.text = text;
        }
        self.text.clone()
    }

    /// Writes the text (`setText`, `interactive-mode.ts:3000`): a busy
    /// engine defers the write.
    fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        let Ok(runner) = self.runner.try_lock() else {
            self.pending_set_text = Some(text.to_string());
            return;
        };
        let _ = runner.component_set_text(self.handle, text);
    }

    /// Re-sets the component's autocomplete provider (`setupAutocomplete
    /// Provider` re-calls it on the non-default editor).
    fn set_autocomplete_provider(&self) {
        let runner = self.runner.lock().expect("extension runner mutex poisoned");
        let _ = runner.component_set_autocomplete_provider(self.handle);
    }

    /// Flushes the deferred keys and text write (the pump, each frame).
    fn flush_pending(&mut self) {
        if !self.pending_input.is_empty() {
            let deferred = std::mem::take(&mut self.pending_input);
            let mut remaining = Vec::new();
            for data in deferred {
                let Ok(mut runner) = self.runner.try_lock() else {
                    remaining.push(data);
                    continue;
                };
                if runner.component_handle_input(self.handle, &data).is_err() {
                    remaining.push(data);
                }
            }
            self.pending_input = remaining;
        }
        if let Some(text) = self.pending_set_text.take() {
            let Ok(runner) = self.runner.try_lock() else {
                self.pending_set_text = Some(text);
                return;
            };
            let _ = runner.component_set_text(self.handle, &text);
        }
    }

    /// Releases the handle (the restore/reset path — upstream drops the
    /// component object; the port drops its registry entry).
    fn release(&self) {
        let runner = self.runner.lock().expect("extension runner mutex poisoned");
        let _ = runner.component_release(self.handle);
    }
}

/// One JS wrapper, seen from Rust (`AutocompleteProviderFactory` wraps the
/// current provider, `types.ts:144`): `suggest` pre-computes the base
/// chain's result for the SAME keystroke and crosses it as data
/// (`__piWrappedSuggest`); a busy engine answers none — a blocking wait
/// here would freeze the pump that must service a dialog (the 6b deadlock
/// class). Deviations from the upstream contract: the base adapter's
/// `getSuggestions` answers with the pre-computed result instead of
/// re-querying, `shouldTriggerFileCompletion` delegates to the base only,
/// and the wrapper's `options.signal` is `undefined`.
#[cfg(feature = "ext-js")]
struct JsAutocompleteProvider {
    runner: Arc<Mutex<pi_ext_js::ExtensionRunner>>,
    index: u32,
    base: Arc<dyn pi_tui::components::editor::AutocompleteProvider>,
}

#[cfg(feature = "ext-js")]
impl pi_tui::components::editor::AutocompleteProvider for JsAutocompleteProvider {
    fn trigger_characters(&self) -> Vec<char> {
        self.base.trigger_characters()
    }

    fn suggest(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<pi_tui::components::editor::Suggestions> {
        let base = self.base.suggest(lines, cursor_line, cursor_col, force);
        let base_json = base
            .as_ref()
            .map(|suggestions| {
                serde_json::json!({
                    "items": suggestions
                        .items
                        .iter()
                        .map(|item| {
                            serde_json::json!({
                                "value": item.value,
                                "label": item.label,
                                "description": item.description,
                            })
                        })
                        .collect::<Vec<_>>(),
                    "prefix": suggestions.prefix,
                })
                .to_string()
            })
            .unwrap_or_default();
        let lines_json = serde_json::to_string(lines).unwrap_or_default();
        let runner = self.runner.try_lock().ok()?;
        let result = runner.wrapped_suggest(
            self.index,
            &base_json,
            &lines_json,
            cursor_line,
            cursor_col,
            force,
        )?;
        let value: serde_json::Value = serde_json::from_str(&result).ok()?;
        let prefix = value
            .get("prefix")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let items = value
            .get("items")?
            .as_array()?
            .iter()
            .filter_map(|item| {
                Some(pi_tui::components::select_list::SelectItem {
                    value: item.get("value")?.as_str()?.to_string(),
                    label: item
                        .get("label")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    description: item
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from),
                })
            })
            .collect();
        Some(pi_tui::components::editor::Suggestions { items, prefix })
    }

    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        self.base
            .should_trigger_file_completion(lines, cursor_line, cursor_col)
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &pi_tui::components::select_list::SelectItem,
        prefix: &str,
    ) -> (Vec<String>, usize, usize) {
        let base = self
            .base
            .apply_completion(lines, cursor_line, cursor_col, item, prefix);
        let base_json = serde_json::json!({
            "lines": base.0,
            "cursorLine": base.1,
            "cursorCol": base.2,
        })
        .to_string();
        let lines_json = serde_json::to_string(lines).unwrap_or_default();
        let item_json = serde_json::json!({
            "value": item.value,
            "label": item.label,
            "description": item.description,
        })
        .to_string();
        // A busy engine, an absent wrapper `applyCompletion`, or an
        // unparsable reply keeps the base's application — upstream only
        // overrides the application when the wrapper defines one
        // (`autocomplete.ts:283-292`).
        let Ok(runner) = self.runner.try_lock() else {
            return base;
        };
        let Some(result) = runner.wrapped_apply(
            self.index,
            &base_json,
            &lines_json,
            cursor_line,
            cursor_col,
            &item_json,
            prefix,
        ) else {
            return base;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&result) else {
            return base;
        };
        let Some(parsed) = (|| {
            let new_lines = value.get("lines")?.as_array()?;
            let new_lines: Vec<String> = new_lines
                .iter()
                .map(|line| line.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()?;
            let new_cursor_line = value.get("cursorLine")?.as_u64()? as usize;
            let new_cursor_col = value.get("cursorCol")?.as_u64()? as usize;
            Some((new_lines, new_cursor_line, new_cursor_col))
        })() else {
            return base;
        };
        parsed
    }
}

/// The trigger-union wrapper (`setupAutocompleteProvider`'s
/// `provider.triggerCharacters = [...new Set(...)]`, `interactive-mode.ts
/// :862-864`): everything delegates; only the trigger characters
/// override.
#[cfg(feature = "ext-js")]
struct ComposedAutocompleteProvider {
    provider: Arc<dyn pi_tui::components::editor::AutocompleteProvider>,
    trigger_characters: Vec<char>,
}

#[cfg(feature = "ext-js")]
impl pi_tui::components::editor::AutocompleteProvider for ComposedAutocompleteProvider {
    fn trigger_characters(&self) -> Vec<char> {
        self.trigger_characters.clone()
    }

    fn suggest(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<pi_tui::components::editor::Suggestions> {
        self.provider.suggest(lines, cursor_line, cursor_col, force)
    }

    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        self.provider
            .should_trigger_file_completion(lines, cursor_line, cursor_col)
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &pi_tui::components::select_list::SelectItem,
        prefix: &str,
    ) -> (Vec<String>, usize, usize) {
        self.provider
            .apply_completion(lines, cursor_line, cursor_col, item, prefix)
    }
}

/// One terminal-input fold attempt (E3 slice 7): the engine was busy (the
/// chunk defers to the pump), a listener consumed the chunk, or this is
/// the transformed chunk the dispatch sees.
#[cfg(feature = "ext-js")]
enum TerminalInputFold {
    Busy,
    Consumed,
    Deliver(String),
}

struct ChatEditor {
    editor: Editor,
    events: Arc<Mutex<Vec<EditorEventOut>>>,
    /// The resolved extension shortcut chords the dispatch checks
    /// (`onExtensionShortcut` upstream — the runner resolution fills this
    /// after load and after every reload).
    extension_shortcuts: Arc<Mutex<Vec<String>>>,
    /// `isBashMode` (`interactive-mode.ts:551`): the draft starts with `!`.
    is_bash_mode: bool,
    /// The active extension editor component (E3 slice 9): the host keeps
    /// the slot; when a component is live, render/input/text route through
    /// it (`this.editor` points at the custom component upstream — the
    /// port's built-in editor stays mounted with its slot, so a dialog
    /// swap or selector park keeps working unchanged).
    #[cfg(feature = "ext-js")]
    extension_editor: Arc<Mutex<Option<ExtensionEditorLive>>>,
}

impl ChatEditor {
    fn new(
        events: Arc<Mutex<Vec<EditorEventOut>>>,
        status: Arc<Mutex<Option<StatusIndicator>>>,
        extension_shortcuts: Arc<Mutex<Vec<String>>>,
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
        ChatEditor {
            editor,
            events,
            extension_shortcuts,
            is_bash_mode: false,
            #[cfg(feature = "ext-js")]
            extension_editor: Arc::new(Mutex::new(None)),
        }
    }
}

impl Component for ChatEditor {
    fn render(&self, width: usize) -> Vec<String> {
        // The extension editor component renders in the slot (E3 slice 9):
        // `this.editor` is the custom component upstream; the port routes
        // the built-in host's render through it.
        #[cfg(feature = "ext-js")]
        if let Some(live) = self
            .extension_editor
            .lock()
            .expect("extension editor")
            .as_ref()
        {
            return live.render(width);
        }
        self.editor.render(width)
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        // The extension editor component owns every key while live (E3
        // slice 9): `this.editor` is the custom component upstream — its
        // `handleInput(data)` is void, so whatever the JS side does with
        // the chunk, nothing forwards to the built-in editor underneath.
        // A busy engine defers the chunk (consumed; the pump redelivers).
        #[cfg(feature = "ext-js")]
        {
            let mut live = self.extension_editor.lock().expect("extension editor");
            if let Some(live) = live.as_mut() {
                let _consumed = live.handle_input(data);
                drop(live);
                return Some(ComponentMutation { consume: true });
            }
        }
        // Extension-registered shortcuts first (`custom-editor.ts:89-92`):
        // the check runs before every built-in key, and the runner's
        // conflict resolution has already filtered the reserved chords out.
        // Gated with the event variant: without `ext-js` nothing ever
        // publishes chords (the slot stays empty), so the check is a no-op.
        #[cfg(feature = "ext-js")]
        {
            let matched = self
                .extension_shortcuts
                .lock()
                .expect("extension shortcuts")
                .iter()
                .find(|key| pi_tui::keys::matches_key(data, key))
                .cloned();
            if let Some(key) = matched {
                self.events
                    .lock()
                    .expect("editor events")
                    .push(EditorEventOut::ExtensionShortcut(key));
                return Some(ComponentMutation { consume: true });
            }
        }

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
        } else if theme::app_keys::matches(data, theme::app_keys::THINKING_CYCLE) {
            Some(AppAction::CycleThinking)
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
        // `defaultEditor.onChange` (`interactive-mode.ts:3248-3253`): the
        // bash-mode flag tracks the draft; a transition repaints the border
        // through the mode's `updateEditorBorderColor`.
        let is_bash_mode = self.editor.get_text().trim_start().starts_with('!');
        if is_bash_mode != self.is_bash_mode {
            self.is_bash_mode = is_bash_mode;
            self.events
                .lock()
                .expect("editor events")
                .push(EditorEventOut::BashModeChanged);
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
    /// The turn's prompts and context: the retry and overflow recovery
    /// re-run the same turn (`agent.continue()`).
    prompts: Vec<AgentMessage>,
    context: AgentContext,
}

/// The retry backoff wait (`_prepareRetry`'s sleep, `agent-session.ts`
/// :3965-3975): a thread sleeps abortably and reports the outcome.
enum RetryWaitEvent {
    Fire,
    Cancelled,
}

struct RetryWait {
    abort: Arc<AbortSignal>,
    events: std::sync::mpsc::Receiver<RetryWaitEvent>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Outcome of a compaction worker thread (`compaction_end` event payload).
enum CompactionOutcome {
    Completed(Box<crate::session::compaction::CompactionResult>),
    Failed(String),
    Cancelled,
}

/// `AgentSessionRuntime.fork`'s position (`agent-session-runtime.ts:264`):
/// `"before"` forks from a user message's parent; `"at"` keeps the entry as
/// the new leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkPosition {
    Before,
    At,
}

impl ForkPosition {
    /// The wire discriminant (`position`, `agent-session-runtime.ts:267`).
    fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::At => "at",
        }
    }
}

/// `AgentSessionRuntime.fork`'s non-error outcomes (`:269-350`): a
/// `session_before_fork` veto, or the completed fork with the selected
/// message's text (`selectedText`, `"before"` only).
enum ForkOutcome {
    Cancelled,
    Completed(Option<String>),
}

/// One staged `/tree` navigation: the confirmed target plus the summarize
/// dialog's decisions, and the branch summary worker while it runs
/// (`showTreeSelector`'s selection tail, `interactive-mode.ts:5766-5871`).
struct TreeNavigationRun {
    target_id: String,
    summarize: bool,
    custom_instructions: Option<String>,
    /// The branch summary worker, while summarizing.
    summary_run: Option<BranchSummaryRun>,
}

/// The branch summary worker thread (`navigateTree`'s generateBranchSummary
/// phase, `agent-session.ts:4230-4263`): the LLM call runs off the UI thread
/// like the compaction worker.
struct BranchSummaryRun {
    abort: Arc<AbortSignal>,
    events: std::sync::mpsc::Receiver<crate::session::branch_summarization::BranchSummaryResult>,
    handle: Option<std::thread::JoinHandle<()>>,
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
///
/// `extension_shortcuts` carries the resolved extension rows as
/// `(raw key, description)` — the `/hotkeys` Extensions section
/// (`interactive-mode.ts:7092-7106`).
#[allow(clippy::too_many_lines)] // mirrors the TS template literal; split when extending
fn hotkeys_markdown(extension_shortcuts: &[(String, String)]) -> String {
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
    let mut hotkeys = format!(
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
    );

    // Extension-registered shortcuts (`interactive-mode.ts:7092-7106`): the
    // display key capitalizes like upstream's `formatKeyText(…,
    // { capitalize: true })`, the description defaults to the extension
    // path.
    if !extension_shortcuts.is_empty() {
        hotkeys += "\n**Extensions**\n| Key | Action |\n|-----|--------|\n";
        for (key, description) in extension_shortcuts {
            let _ = writeln!(
                hotkeys,
                "| `{}` | {description} |",
                theme::format_chord(key)
            );
        }
    }
    hotkeys
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

/// The wire name of a level recorded on an assistant message (TS
/// `ThinkingLevel`'s string union; `pi-ai`'s enum serializes camelCase).
fn recorded_level_name(level: pi_ai::types::ThinkingLevel) -> &'static str {
    match level {
        pi_ai::types::ThinkingLevel::Minimal => "minimal",
        pi_ai::types::ThinkingLevel::Off => "off",
        pi_ai::types::ThinkingLevel::Low => "low",
        pi_ai::types::ThinkingLevel::Medium => "medium",
        pi_ai::types::ThinkingLevel::High => "high",
    }
}

/// `updateAvailableProviderCount` (`interactive-mode.ts:5444-5451`): the
/// unique providers among the available models — upstream the
/// `scopedModels` list, else `modelRuntime.getAvailableSnapshot()`, which
/// keeps the composed catalog's models on providers with configured
/// credentials (`model-runtime.ts:332-341` `snapshot.available`). The port's
/// snapshot is the composed catalog (`models_state::all`); availability is
/// the provider's resolvable wire credential, the same ladder the requests
/// use.
fn available_provider_count(auth_path: &std::path::Path) -> usize {
    let mut providers: Vec<String> = Vec::new();
    for model in crate::models_state::all() {
        if !providers.iter().any(|existing| existing == &model.provider) {
            providers.push(model.provider.clone());
        }
    }
    providers
        .into_iter()
        .filter(|provider| {
            crate::modes::print_mode::resolve_wire_credential(provider, None, auth_path).is_some()
        })
        .count()
}

/// The interactive mode (`InteractiveMode`, reduced to the chat core).
// The booleans mirror upstream TS state flags; the port keeps them one-to-one.
#[allow(clippy::struct_excessive_bools)]
pub struct InteractiveMode {
    tui: Tui,
    editor_events: Arc<Mutex<Vec<EditorEventOut>>>,
    status_handle: Arc<Mutex<Option<StatusIndicator>>>,
    /// The resolved extension shortcut chords the editor dispatch checks
    /// (the mode's half of `onExtensionShortcut` — refilled after load and
    /// after every reload).
    editor_extension_shortcuts: Arc<Mutex<Vec<String>>>,
    /// The shared markdown-transform plumbing (E3 slice 2): cloned into
    /// every markdown component so extension transformers run at render
    /// time (`getMarkdownTransformers` upstream).
    markdown_transform_ctx: super::components::markdown_transform::MarkdownTransformCtx,
    /// The shared chain the setup path refills (only with `ext-js`).
    #[cfg(feature = "ext-js")]
    markdown_transform_chain: Arc<Mutex<Vec<pi_ext_js::ResolvedMarkdownTransformer>>>,

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
    /// The `agent_end` verdict, consumed by the post-run machine.
    last_agent_end_will_retry: bool,
    /// The `agent_end` messages, consumed by the post-run machine.
    last_agent_end_messages: Option<Vec<AgentMessage>>,
    /// The retry budget so far (`_retryAttempt`).
    retry_attempt: u32,
    /// The retry backoff in flight, if any.
    retry_wait: Option<RetryWait>,
    /// The retired turn's prompts and context for a re-run.
    last_run_turn: Option<(Vec<AgentMessage>, AgentContext)>,
    /// The automatic compaction re-runs the turn on completion.
    auto_continue_after_compaction: bool,
    /// Messages queued while compaction runs
    /// (`compactionQueuedMessages`), `(text, follow_up)`.
    compaction_queued: Vec<(String, bool)>,
    /// ctrl+c twice within 500ms exits (`handleCtrlC`).
    last_sigint_time: Option<Instant>,
    /// The chat-tail status pair's identity (`lastStatusSpacer` +
    /// `lastStatusText`, `interactive-mode.ts:519-521`): heap addresses of
    /// the two rows, immune to index shifts from streaming splices.
    /// Back-to-back `show_status` calls update the pair in place instead of
    /// stacking rows.
    last_status_pair: Option<(usize, usize)>,
    /// Spinner-frame pacing, carried across `pump` calls.
    last_spinner_tick: Instant,
    /// ctrl+o state: tool output expansion and the header hints.
    tool_output_expanded: bool,
    hide_thinking_block: bool,
    /// The working-status state the extension surfaces drive
    /// (`workingMessage`/`workingVisible`/`workingIndicatorOptions`/
    /// `hiddenThinkingLabel`, `interactive-mode.ts:504-507`); the defaults
    /// live in the setters.
    working_message: Option<String>,
    working_visible: bool,
    working_indicator_options: Option<pi_tui::components::loader::LoaderIndicator>,
    hidden_thinking_label: String,
    /// The extension widgets above/below the editor (`setWidget`, E3
    /// slice 7 — `extensionWidgetsAbove`/`extensionWidgetsBelow`,
    /// `interactive-mode.ts:602-603`), keyed by the `setWidget` key,
    /// insertion order preserved.
    #[cfg(feature = "ext-js")]
    extension_widgets_above: Vec<(String, ExtensionWidget)>,
    #[cfg(feature = "ext-js")]
    extension_widgets_below: Vec<(String, ExtensionWidget)>,
    /// The custom footer/header swap (`customFooter`/`customHeader`, E3
    /// slice 7): the adopted handle the registry owns and the displaced
    /// built-in component.
    #[cfg(feature = "ext-js")]
    custom_footer: Option<ExtensionChrome>,
    #[cfg(feature = "ext-js")]
    custom_header: Option<ExtensionChrome>,
    /// The mode-state snapshot the synchronous ui ops read (E3 slice 7):
    /// refreshed per pump frame, read by the runner's closures.
    #[cfg(feature = "ext-js")]
    ui_snapshot: Arc<Mutex<UiSnapshotState>>,
    /// The terminal-input listener count the crossings sync, and the
    /// chunks deferred while the engine held the runner mutex (E3 slice 7).
    #[cfg(feature = "ext-js")]
    terminal_input_listeners: usize,
    #[cfg(feature = "ext-js")]
    deferred_terminal_input: Vec<String>,
    /// The active extension editor component (E3 slice 9): the same state
    /// Arc the editor host routes render/input through; `None` while the
    /// built-in editor is live.
    #[cfg(feature = "ext-js")]
    extension_editor: Arc<Mutex<Option<ExtensionEditorLive>>>,
    /// Whether the working-status row is mounted in the status host (E3
    /// slice 9 — the delegate never embeds the status, so it renders in
    /// the host row like upstream's non-embedded branch).
    #[cfg(feature = "ext-js")]
    extension_editor_status_row: bool,
    /// The boot-frozen base autocomplete provider the JS wrapper chain
    /// wraps (`createBaseAutocompleteProvider`'s product; upstream
    /// re-creates it per setup, E3 slice 8 keeps the boot-frozen base).
    base_autocomplete_provider: Arc<dyn pi_tui::components::editor::AutocompleteProvider>,
    /// The registered `addAutocompleteProvider` wrappers, in registration
    /// order (`autocompleteProviderWrappers`, `interactive-mode.ts:485`).
    #[cfg(feature = "ext-js")]
    autocomplete_wrappers: Vec<AutocompleteWrapperRecord>,
    /// The message rows' horizontal padding (`this.outputPad`,
    /// `interactive-mode.ts`), read from `outputPad` at boot and applied by
    /// the output-padding row.
    output_pad: usize,
    exiting: bool,
    /// The mounted `/model` selector's event queue, while open.
    model_selector_events: Option<Arc<Mutex<Vec<ModelSelectorEvent>>>>,
    /// The mounted `/scoped-models` selector's event queue, while open.
    scoped_models_selector_events: Option<Arc<Mutex<Vec<ScopedModelsSelectorEvent>>>>,
    /// `AgentSession._scopedModels` (`agent-session.ts:414`): the session's
    /// model scope — seeded from the `enabledModels` setting (upstream also
    /// resolves the `--models` flag; the port has no such flag yet), changed
    /// by `/scoped-models`, and honored by ctrl+p cycling, the `/model`
    /// completions, and the footer's provider count. The entries' explicit
    /// thinking levels apply on the scoped switch (`_cycleScopedModel`,
    /// `agent-session.ts:2709-2745`).
    scoped_models: Vec<ScopedModel>,
    /// The `/model` argument-completion source's live view of the scope
    /// (upstream's provider closure reads `session.scopedModels` per call,
    /// `interactive-mode.ts:765-767`; the port's provider is built once, so
    /// the scope rides a shared slot the scope mutations refresh).
    scoped_completion_models: Arc<Mutex<Vec<pi_ai::types::Model>>>,
    /// Events from the mounted `/trust` selector, same queue pattern.
    trust_selector_events: Option<Arc<Mutex<Vec<TrustSelectorEvent>>>>,
    /// Events from the mounted `/settings` dialog, same queue pattern.
    settings_selector_events: Option<Arc<Mutex<Vec<SettingsSelectorEvent>>>>,
    session_selector_events: Option<Arc<Mutex<Vec<SessionSelectorEvent>>>>,
    /// Events from the mounted `/import` dialogs (`showExtensionConfirm` and
    /// `promptForMissingSessionCwd`, `interactive-mode.ts:6762`/`:6778`) —
    /// the generic selector with a pending decision in [`ImportDialog`].
    import_selector_events: Option<Arc<Mutex<Vec<ExtensionSelectorEvent>>>>,
    import_dialog: Option<ImportDialog>,
    /// The interactive `ctx.ui` bridge (E3 slice 6a): the sink
    /// `extensions::load` bound, with the pump polling its request
    /// channel (`createExtensionUIContext` over the protocol seam).
    #[cfg(feature = "ext-js")]
    extension_ui_bridge: Option<Arc<super::extension_ui::ExtensionUiBridge>>,
    /// The extension dialog stack (`extensionDialogs`,
    /// `interactive-mode.ts:584`): a new dialog hides the visible one and
    /// closing the visible one shows the previous again; every dialog
    /// stays answerable. The top's component rides the editor slot; the
    /// rest park in their entries.
    #[cfg(feature = "ext-js")]
    extension_ui_dialogs: Vec<ExtensionUiDialogEntry>,
    /// The extension overlays (`extensionOverlays`,
    /// `interactive-mode.ts:590-591`): overlay-mode customs and the factory
    /// `tui.showOverlay` surface, removed by `done()`/reset — never by a
    /// session switch.
    #[cfg(feature = "ext-js")]
    extension_overlays: Vec<ExtensionOverlayEntry>,
    /// Numbers the program-status sources of blocking overlays, so closing
    /// one keeps the others (`interactive-mode.ts:586`).
    #[cfg(feature = "ext-js")]
    next_blocking_overlay_id: u32,
    /// The OSC 7501 program status reporter (`interactive-mode.ts:572-576`):
    /// `working`/`blocked`/`done`/`error` reports ride every agent event and
    /// the dialogs' `blocked` sources (`program-status-reporter.ts`).
    program_status: ProgramStatusReporter,
    /// Events from the mounted `/fork` dialog (`UserMessageSelectorComponent`'s
    /// onSelect/onCancel).
    user_message_selector_events: Option<Arc<Mutex<Vec<UserMessageSelectorEvent>>>>,
    /// Events from the mounted `/tree` selector
    /// (`TreeSelectorComponent`'s onSelect/onCancel/onCopy/onLabelChange).
    tree_selector_events: Option<Arc<Mutex<Vec<TreeSelectorEvent>>>>,
    /// Events from the mounted "Summarize branch?" dialog
    /// (`showExtensionSelector`'s resolve).
    tree_summary_dialog_events: Option<Arc<Mutex<Vec<ExtensionSelectorEvent>>>>,
    /// Events from the mounted custom-summarization-prompt editor
    /// (`showExtensionEditor`'s resolve).
    tree_prompt_editor_events: Option<Arc<Mutex<Vec<ExtensionEditorEvent>>>>,
    /// The staged `/tree` navigation: the confirmed target, the dialog
    /// decisions, and the branch summary worker while it runs.
    tree_navigation: Option<TreeNavigationRun>,
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
    /// The running extension-oauth login flow (slice 6j).
    #[cfg(feature = "ext-js")]
    extension_login_flow: Option<ExtensionLoginFlow>,
    /// Events from the mounted extension-login text prompt (the
    /// `onPrompt`/`onManualCodeInput` asks).
    #[cfg(feature = "ext-js")]
    extension_login_prompt: Option<Arc<Mutex<Vec<LoginPromptEvent>>>>,
    /// Events from the mounted extension-login option selector (the
    /// `onSelect` ask).
    #[cfg(feature = "ext-js")]
    extension_login_select: Option<Arc<Mutex<Vec<AuthSelectorEvent>>>>,
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
    /// The CLI `--extension` paths (`/reload` re-resolves them with the
    /// reloaded settings, `resource-loader.ts:557-560`).
    #[cfg(feature = "ext-js")]
    extension_cli_paths: Vec<String>,
    /// The `--no-extensions` flag — CLI extensions still load.
    #[cfg(feature = "ext-js")]
    no_extensions: bool,
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
        let editor_extension_shortcuts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut editor_host = ChatEditor::new(
            Arc::clone(&editor_events),
            Arc::clone(&status_handle),
            Arc::clone(&editor_extension_shortcuts),
        );
        // The extension editor state is shared between the host (render/
        // input) and the mode (pump arms, swaps) — E3 slice 9.
        #[cfg(feature = "ext-js")]
        let extension_editor_state = Arc::clone(&editor_host.extension_editor);

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
        // The `/model` completions' live scope view (`createBaseAutocompleteProvider`'s
        // closure reads `session.scopedModels` per call, `interactive-mode.ts:765`).
        let scoped_completion_models: Arc<Mutex<Vec<pi_ai::types::Model>>> =
            Arc::new(Mutex::new(Vec::new()));
        let provider = autocomplete::CombinedAutocompleteProvider::new(
            Self::autocomplete_commands(
                &virtual_catalog_models,
                Arc::clone(&scoped_completion_models),
            ),
            runtime.session.get_cwd().to_path_buf(),
            crate::tools::bash::which("fd"),
        );
        // The base stays reachable for the wrapper-chain rebuilds (E3
        // slice 8): upstream re-creates it per `setupAutocompleteProvider`;
        // the port's base is boot-frozen as before, and the JS wrappers
        // chain onto this Arc.
        let base_autocomplete_provider: Arc<dyn pi_tui::components::editor::AutocompleteProvider> =
            Arc::new(provider);
        editor_host
            .editor
            .set_autocomplete_provider(Some(Arc::clone(&base_autocomplete_provider)));

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
        let (agent_dir, settings, hide_thinking_block, output_pad, queues) =
            Self::startup_settings(options, runtime);
        // The renderer choice: the `--tui-mode` flag wins, otherwise the
        // settings (`getTuiMode`, default fullscreen).
        let tui_mode = options.tui_mode.unwrap_or_else(|| settings.get_tui_mode());

        // The shared markdown-transform plumbing (E3 slice 2): one context
        // cloned into every markdown component, the chain refilled at setup
        // and `/reload` (`getMarkdownTransformers` upstream).
        #[cfg(feature = "ext-js")]
        let (markdown_transform_ctx, markdown_transform_chain) =
            super::components::markdown_transform::MarkdownTransformCtx::for_runner(
                runtime.extension_runner.clone(),
            );
        #[cfg(not(feature = "ext-js"))]
        let markdown_transform_ctx =
            super::components::markdown_transform::MarkdownTransformCtx::default();

        let mut mode = InteractiveMode {
            tui,
            editor_events,
            status_handle,
            editor_extension_shortcuts,
            markdown_transform_ctx,
            #[cfg(feature = "ext-js")]
            markdown_transform_chain,
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
            last_status_pair: None,
            last_spinner_tick: Instant::now(),
            tool_output_expanded: false,
            hide_thinking_block,
            working_message: None,
            working_visible: true,
            working_indicator_options: None,
            hidden_thinking_label: "Thinking...".to_string(),
            #[cfg(feature = "ext-js")]
            extension_widgets_above: Vec::new(),
            #[cfg(feature = "ext-js")]
            extension_widgets_below: Vec::new(),
            #[cfg(feature = "ext-js")]
            custom_footer: None,
            #[cfg(feature = "ext-js")]
            custom_header: None,
            #[cfg(feature = "ext-js")]
            ui_snapshot: Arc::new(Mutex::new(UiSnapshotState::default())),
            #[cfg(feature = "ext-js")]
            terminal_input_listeners: 0,
            #[cfg(feature = "ext-js")]
            deferred_terminal_input: Vec::new(),
            #[cfg(feature = "ext-js")]
            extension_editor: extension_editor_state,
            #[cfg(feature = "ext-js")]
            extension_editor_status_row: false,
            base_autocomplete_provider,
            #[cfg(feature = "ext-js")]
            autocomplete_wrappers: Vec::new(),
            output_pad,
            exiting: false,
            #[cfg(feature = "ext-js")]
            extension_cli_paths: options.extension_cli_paths.clone(),
            #[cfg(feature = "ext-js")]
            no_extensions: options.no_extensions,
            model_selector_events: None,
            scoped_models_selector_events: None,
            scoped_models: Vec::new(),
            scoped_completion_models,
            trust_selector_events: None,
            settings_selector_events: None,
            session_selector_events: None,
            import_selector_events: None,
            import_dialog: None,
            #[cfg(feature = "ext-js")]
            extension_ui_bridge: options.extension_ui.clone(),
            #[cfg(feature = "ext-js")]
            extension_ui_dialogs: Vec::new(),
            #[cfg(feature = "ext-js")]
            extension_overlays: Vec::new(),
            #[cfg(feature = "ext-js")]
            next_blocking_overlay_id: 0,
            program_status: ProgramStatusReporter::new(),
            user_message_selector_events: None,
            tree_selector_events: None,
            tree_summary_dialog_events: None,
            tree_prompt_editor_events: None,
            tree_navigation: None,
            auth_selector_events: None,
            auth_selector_mode: None,
            pending_auth_type: None,
            pending_auth_type_options: None,
            login_prompt_events: None,
            login_prompt_provider: None,
            login_back: None,
            #[cfg(feature = "ext-js")]
            extension_login_flow: None,
            #[cfg(feature = "ext-js")]
            extension_login_prompt: None,
            #[cfg(feature = "ext-js")]
            extension_login_select: None,
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
            last_agent_end_will_retry: false,
            last_agent_end_messages: None,
            retry_attempt: 0,
            retry_wait: None,
            last_run_turn: None,
            auto_continue_after_compaction: false,
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
        };
        // The session's initial scope (`main.ts:871-882`): the `enabledModels`
        // setting resolves against the available snapshot; upstream rides the
        // `--models` flag through the same resolver.
        mode.seed_model_scope_from_settings();
        // Startup border color (`interactive-mode.ts:2241`).
        mode.update_editor_border_color();
        // `this.footer.setAutoCompactEnabled(this.session.autoCompactionEnabled)`
        // (`interactive-mode.ts:686`): the boot value of the footer's
        // auto-compact indicator; the mount predates the settings load.
        let auto_compact = mode.settings.get_compaction_enabled();
        if let Some(footer) = mode.downcast_tree::<FooterComponent>(HostSlot::Footer) {
            footer.data.auto_compact_enabled = auto_compact;
        }
        // The extension components' theme + keybinding surface (E3 slice 3):
        // `theme.fg`/`theme.bg` read the live port theme per call — the same
        // module-singleton behavior as upstream's `theme.ts:297/:303` — so a
        // `/theme` switch needs no rebind, and the keybindings table feeds
        // the renderers' `keyText`/`keyHint` (`keybinding-hints.ts:34-44`).
        #[cfg(feature = "ext-js")]
        if let Some(runner) = mode.extension_runner.as_ref() {
            let fg = std::sync::Arc::new(|token: &str, text: &str| {
                match pi_tui::theme::ThemeColor::from_name(token) {
                    Some(color) => theme::theme().fg(color, text),
                    None => text.to_string(),
                }
            });
            let bg = std::sync::Arc::new(|token: &str, text: &str| {
                match pi_tui::theme::ThemeBg::from_name(token) {
                    Some(color) => theme::theme().bg(color, text),
                    None => text.to_string(),
                }
            });
            // The factory's `theme.bold/italic/underline/inverse/
            // strikethrough` (E3 slice 6b): the attribute wrappers of the
            // live theme, exactly what upstream's `theme.ts:309-321` calls
            // resolve to.
            let style = std::sync::Arc::new(|method: &str, text: &str| {
                let attributes = match method {
                    "bold" => pi_tui::color::TextAttributes {
                        bold: true,
                        ..pi_tui::color::TextAttributes::default()
                    },
                    "italic" => pi_tui::color::TextAttributes {
                        italic: true,
                        ..pi_tui::color::TextAttributes::default()
                    },
                    "underline" => pi_tui::color::TextAttributes {
                        underline: true,
                        ..pi_tui::color::TextAttributes::default()
                    },
                    "inverse" => pi_tui::color::TextAttributes {
                        inverse: true,
                        ..pi_tui::color::TextAttributes::default()
                    },
                    "strikethrough" => pi_tui::color::TextAttributes {
                        strikethrough: true,
                        ..pi_tui::color::TextAttributes::default()
                    },
                    _ => return text.to_string(),
                };
                pi_tui::color::style_text_with_ansi(text, None, None, attributes)
            });
            let keybindings: std::collections::BTreeMap<&str, Vec<&str>> =
                theme::app_keys::DEFAULTS
                    .iter()
                    .map(|(id, keys)| (*id, keys.to_vec()))
                    .collect();
            let mut guard = runner.lock().expect("extension runner mutex poisoned");
            guard.set_theme_surface(fg, bg);
            guard.set_style_surface(style);
            guard.set_app_keybindings(
                &serde_json::to_string(&keybindings).unwrap_or_else(|_| "{}".to_string()),
            );
            // The mode-state snapshot the synchronous ui queries read (E3
            // slice 7): footer data, the editor's text, and the tools flag —
            // closures over the shared snapshot the pump refreshes.
            let footer_snapshot = Arc::clone(&mode.ui_snapshot);
            let editor_snapshot = Arc::clone(&mode.ui_snapshot);
            let tools_snapshot = Arc::clone(&mode.ui_snapshot);
            let chrome_snapshot = Arc::clone(&mode.ui_snapshot);
            let border_snapshot = Arc::clone(&mode.ui_snapshot);
            guard.set_ui_snapshot_surface(pi_ext_js::UiSnapshotSurface {
                footer_data: std::sync::Arc::new(move || {
                    footer_snapshot.lock().expect("ui snapshot").footer.clone()
                }),
                editor_text: std::sync::Arc::new(move || {
                    editor_snapshot
                        .lock()
                        .expect("ui snapshot")
                        .editor_text
                        .clone()
                }),
                tools_expanded: std::sync::Arc::new(move || {
                    tools_snapshot.lock().expect("ui snapshot").tools_expanded
                }),
                editor_chrome: std::sync::Arc::new(move || {
                    let snapshot = chrome_snapshot.lock().expect("ui snapshot");
                    serde_json::json!({
                        "paddingX": snapshot.editor_padding_x,
                        "maxVisible": snapshot.editor_max_visible,
                        "appActions": snapshot.app_actions,
                    })
                }),
                editor_border_color: std::sync::Arc::new(move || {
                    border_snapshot
                        .lock()
                        .expect("ui snapshot")
                        .editor_border_color
                        .clone()
                }),
            });
        }
        // Extension shortcuts (`setupExtensionShortcuts`,
        // `interactive-mode.ts:2170/:2381-2432`): resolve against the
        // resolved keybindings, publish the chords to the editor, and
        // surface the conflict warnings.
        #[cfg(feature = "ext-js")]
        mode.setup_extension_shortcuts();
        // The markdown transformers resolve on the same lifecycle
        // (`getMarkdownTransformers` reads the runner live upstream; the
        // chain only changes at load/reload).
        #[cfg(feature = "ext-js")]
        mode.setup_markdown_transformers();
        // The initial program status report (`interactive-mode.ts:1040`,
        // `init` after `ui.start()`): an idle report now is re-written once
        // the terminal confirms OSC 7501 support. The init title follows
        // (`interactive-mode.ts:2242`).
        let session_name = mode.session.get_session_name();
        mode.program_status
            .report(session_name.as_deref(), &mut mode.tui);
        mode.update_terminal_title();
        mode
    }

    /// Resolves the extension shortcuts against the port's default
    /// keybindings and wires the editor dispatch
    /// (`setupExtensionShortcuts`, `interactive-mode.ts:2381-2432`): the
    /// chords publish to the shared editor slot, the conflict warnings
    /// print as `Warning:` lines (the port's loader-line diagnostics
    /// channel — upstream feeds the `[Extension issues]` banner), and the
    /// handlers dispatch through the runner on the editor event.
    #[cfg(feature = "ext-js")]
    fn setup_extension_shortcuts(&mut self) {
        let Some(runner) = &self.extension_runner else {
            return;
        };
        let (keys, warnings) = {
            let mut runner = runner.lock().expect("extension runner mutex poisoned");
            runner.get_shortcuts(theme::app_keys::DEFAULTS);
            let keys = runner
                .shortcuts()
                .iter()
                .map(|shortcut| shortcut.key.clone())
                .collect::<Vec<_>>();
            let warnings = runner
                .shortcut_diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.message.clone())
                .collect::<Vec<_>>();
            (keys, warnings)
        };
        *self
            .editor_extension_shortcuts
            .lock()
            .expect("extension shortcuts") = keys;
        for message in &warnings {
            eprintln!("Warning: {message}");
        }
    }

    /// Refills the shared markdown-transform chain from the runner
    /// (`getMarkdownTransformers`, `runner.ts:793-803` — load order; the
    /// surface filter runs at application time).
    #[cfg(feature = "ext-js")]
    fn setup_markdown_transformers(&mut self) {
        let Some(runner) = &self.extension_runner else {
            return;
        };
        let resolved = runner
            .lock()
            .expect("extension runner mutex poisoned")
            .get_markdown_transformers();
        *self
            .markdown_transform_chain
            .lock()
            .expect("markdown transformers") = resolved;
    }

    /// Runs one resolved extension shortcut (`interactive-mode.ts:2425-2431`):
    /// upstream runs the handler async off the input path and a rejection
    /// surfaces as `Shortcut handler error: …`; the port dispatches through
    /// the runner and reports the same text.
    #[cfg(feature = "ext-js")]
    fn run_extension_shortcut(&mut self, key: &str) {
        let Some(runner) = &self.extension_runner else {
            return;
        };
        let result = runner
            .lock()
            .expect("extension runner mutex poisoned")
            .run_shortcut(key);
        if let Err(error) = result {
            self.show_error(&format!("Shortcut handler error: {error}"));
        }
    }

    /// The `/hotkeys` Extensions rows (`interactive-mode.ts:7098-7105`):
    /// `(raw key, description ?? extension path)` per resolved shortcut —
    /// `hotkeys_markdown` applies the capitalize.
    #[cfg(feature = "ext-js")]
    fn extension_shortcut_rows(&self) -> Vec<(String, String)> {
        let Some(runner) = &self.extension_runner else {
            return Vec::new();
        };
        runner
            .lock()
            .expect("extension runner mutex poisoned")
            .shortcuts()
            .iter()
            .map(|shortcut| {
                let description = if shortcut.description.is_empty() {
                    shortcut.extension_path.clone()
                } else {
                    shortcut.description.clone()
                };
                (shortcut.key.clone(), description)
            })
            .collect()
    }

    /// Seed the session scope from the `enabledModels` setting
    /// (`main.ts:871-882`): the resolved models + the pattern diagnostics,
    /// which upstream returns as startup diagnostics (`main.ts:882-883`) and
    /// the port prints as the loader's warning lines.
    fn seed_model_scope_from_settings(&mut self) {
        let Some(patterns) = self.settings.get_enabled_models() else {
            return;
        };
        if patterns.is_empty() {
            return;
        }
        let available = self.available_models();
        let resolved = resolve_model_scope_from_models(&patterns, &available);
        for diagnostic in &resolved.diagnostics {
            eprintln!("Warning: {}", diagnostic.message);
        }
        self.set_scoped_models(resolved.scoped_models);
    }

    /// `setScopedModels` (`agent-session.ts:1763-1766`) plus the port's live
    /// completion slot: the scope state and the `/model` completions move
    /// together.
    fn set_scoped_models(&mut self, scoped_models: Vec<ScopedModel>) {
        self.scoped_models = scoped_models;
        let snapshot: Vec<pi_ai::types::Model> = self
            .scoped_models
            .iter()
            .map(|scoped| scoped.model.clone())
            .collect();
        *self
            .scoped_completion_models
            .lock()
            .expect("scoped completion models") = snapshot;
    }

    /// The regular renderer's flat mount: the eight hosts in [`HostSlot`]
    /// order (`interactive-mode.ts` init's `mountInteractiveTui` flat path,
    /// with the widget hosts E3 slice 7 adds), focus on the editor.
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
        // The widget hosts (`widgetContainerAbove`/`widgetContainerBelow`,
        // `interactive-mode.ts:670-671`) — the above host boots with its
        // spacer (`renderWidgets()`, `interactive-mode.ts:1004`).
        let mut widget_above = pi_tui::component::Container::new();
        widget_above.add_child(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        tui.add_child(Box::new(widget_above));
        // Ids are heap addresses, so the editor's identity is taken once the
        // host is boxed — focus names it across the mode switches.
        let editor_box = Box::new(editor_host);
        let editor_id = component_id(editor_box.as_ref());
        tui.add_child(editor_box);
        tui.add_child(Box::new(pi_tui::component::Container::new()));
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
        usize,
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
        let output_pad = usize::try_from(settings.get_output_pad()).unwrap_or(1);
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
        (agent_dir, settings, hide_thinking_block, output_pad, queues)
    }

    /// The `/`-commands offered by autocomplete (`createBaseAutocompleteProvider`,
    /// `interactive-mode.ts:743-853`). Only the commands this port implements
    /// are listed — advertising upstream's unimplemented builtins would make
    /// the picker lie. Argument completions: `/model` fuzzy-searches the
    /// session's scoped models when a scope is set, else the whole catalog
    /// (`interactive-mode.ts:764-767`), `/thinking` the valid levels.
    #[allow(clippy::too_many_lines)] // mirrors the TS command registry; split when extending
    fn autocomplete_commands(
        virtual_models: &[pi_ai::types::Model],
        scoped_completion_models: Arc<Mutex<Vec<pi_ai::types::Model>>>,
    ) -> Vec<autocomplete::SlashCommand> {
        let virtual_models = virtual_models.to_vec();
        let commands = vec![
            autocomplete::SlashCommand::new("model", "Select model (opens selector UI)")
                .with_argument_hint("<provider/model>")
                .with_argument_completions(Arc::new(move |prefix: &str| {
                    // The scoped subset wins over the whole snapshot
                    // (`interactive-mode.ts:765-767`).
                    let scoped = scoped_completion_models
                        .lock()
                        .expect("scoped completion models")
                        .clone();
                    let composed = crate::models_state::all();
                    let mut models: Vec<(String, String, String)> = Vec::new(); // (id, provider, name)
                    if scoped.is_empty() {
                        for provider in crate::models_state::providers() {
                            for model in composed.iter().filter(|model| model.provider == provider)
                            {
                                // A virtual registration hides the same-id
                                // physical entry (`withVirtualModels`).
                                if virtual_models.iter().any(|virtual_model| {
                                    virtual_model.provider == provider
                                        && virtual_model.id == model.id
                                }) {
                                    continue;
                                }
                                models.push((
                                    model.id.clone(),
                                    provider.clone(),
                                    model.name.clone(),
                                ));
                            }
                        }
                        for model in &virtual_models {
                            models.push((
                                model.id.clone(),
                                model.provider.clone(),
                                model.name.clone(),
                            ));
                        }
                    } else {
                        for model in &scoped {
                            models.push((
                                model.id.clone(),
                                model.provider.clone(),
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
            autocomplete::SlashCommand::new("trust", "Show the project trust options"),
            autocomplete::SlashCommand::new("settings", "Open settings menu"),
            autocomplete::SlashCommand::new("resume", "Resume another session"),
            // `BUILTIN_SLASH_COMMANDS` (`slash-commands.ts:20-43`) descriptions
            // and hints, verbatim. `/bug` stays unadvertised: its flow needs
            // the bug-report bundle, upload, and archive infrastructure.
            // `/share` (`slash-commands.ts:27`) is deliberately absent: it
            // uploads to the upstream Radius gateway, which this repo bans.
            autocomplete::SlashCommand::new(
                "export",
                "Export session (HTML default, or specify path: .html/.jsonl)",
            ),
            autocomplete::SlashCommand::new(
                "import",
                "Import a JSONL session, preserving its ID and working directory",
            ),
            autocomplete::SlashCommand::new("copy", "Copy last agent message to clipboard"),
            autocomplete::SlashCommand::new("name", "Set session display name"),
            autocomplete::SlashCommand::new("session", "Show session info and stats"),
            autocomplete::SlashCommand::new("changelog", "Show changelog entries"),
            autocomplete::SlashCommand::new("hotkeys", "Show all keyboard shortcuts"),
            // `slash-commands.ts:34-35`.
            autocomplete::SlashCommand::new(
                "fork",
                "Create a new fork from a previous user message",
            ),
            autocomplete::SlashCommand::new(
                "clone",
                "Duplicate the current session at the current position",
            ),
            autocomplete::SlashCommand::new("tree", "Navigate session tree (switch branches)"),
            // `slash-commands.ts:24`.
            autocomplete::SlashCommand::new(
                "scoped-models",
                "Enable/disable models for Ctrl+P cycling",
            ),
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
            // `slash-commands.ts:42`.
            autocomplete::SlashCommand::new(
                "reload",
                "Reload keybindings, extensions, skills, prompts, themes, and context files",
            ),
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
            available_provider_count: available_provider_count(&crate::config::get_auth_path()),
            routed_model_id: None,
            routed_thinking_level: None,
            auto_compact_enabled: false,
            extension_statuses: std::collections::BTreeMap::new(),
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

    /// A chat row's heap address (upstream's object identity for
    /// `lastStatusText`/`lastStatusSpacer`): stable across the index shifts
    /// streaming splices cause.
    fn chat_row_addr(&mut self, index: usize) -> Option<usize> {
        let chat = self.chat_mut();
        chat.get(index).map(|row| {
            std::ptr::from_ref::<dyn pi_tui::component::Component>(row).cast::<()>() as usize
        })
    }

    fn editor_mut(&mut self) -> &mut ChatEditor {
        self.downcast_tree::<ChatEditor>(HostSlot::Editor)
            .expect("editor host")
    }

    /// The active editor's `addToHistory?.()` (E3 slice 9): the delegate's
    /// duck-typed crossing when a component is live (a no-op when it lacks
    /// the method), the built-in editor's otherwise.
    fn editor_add_to_history(&mut self, text: &str) {
        #[cfg(feature = "ext-js")]
        {
            let target = self
                .extension_editor
                .lock()
                .expect("extension editor")
                .as_ref()
                .map(|live| (Arc::clone(&live.runner), live.handle));
            if let Some((runner, handle)) = target {
                let _ = runner.component_add_to_history(handle, text);
                return;
            }
        }
        self.editor_mut().editor.add_to_history(text);
    }

    /// The editor host, mounted or parked: `this.editor` stays reachable
    /// while a selector occupies the slot, because the parked editor rides
    /// in `suspended_editor`.
    fn editor_host_mut(&mut self) -> Option<&mut ChatEditor> {
        let mounted = self.downcast_tree::<ChatEditor>(HostSlot::Editor).is_some();
        if mounted {
            return self.downcast_tree::<ChatEditor>(HostSlot::Editor);
        }
        let suspended = self.suspended_editor.as_mut()?;
        suspended.as_any_mut()?.downcast_mut::<ChatEditor>()
    }

    // --------------------------------------------------------------
    // Message rendering (`addMessageToChat`, `interactive-mode.ts:4008`)
    // --------------------------------------------------------------

    fn add_user_message(&mut self, text: &str) {
        if self.chat_len() > 0 {
            self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        }
        self.add_to_chat(Box::new(UserMessageComponent::new(
            text,
            self.output_pad,
            self.markdown_transform_ctx.clone(),
        )));
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

    /// `addCustomEntryToChat` (`interactive-mode.ts:3986-4007`): the FIRST
    /// extension with a renderer for the type runs it through the engine;
    /// without one the entry is skipped entirely, and a renderer that
    /// produces nothing leaves no row. During a run the row splices ahead
    /// of the streaming assistant line (`:3997-4003`), shifting the port's
    /// tracked chat indices one slot down.
    #[cfg(feature = "ext-js")]
    fn add_custom_entry_to_chat(&mut self, entry: &crate::session::records::CustomEntry) {
        let Some(runner) = self.extension_runner.as_ref() else {
            return;
        };
        let ext_index = {
            let runner = runner.lock().expect("extension runner mutex poisoned");
            match runner.get_entry_renderer(&entry.custom_type) {
                Some(index) => index,
                None => return,
            }
        };
        // The renderer receives the on-disk record — `type` included.
        let entry_json = {
            let mut value = serde_json::to_value(entry).expect("custom entry serializes");
            if let Some(object) = value.as_object_mut() {
                object.insert("type".to_string(), serde_json::Value::from("custom"));
            }
            serde_json::to_string(&value).expect("custom entry serializes")
        };
        let component = super::components::custom_entry::CustomEntryComponent::new(
            std::sync::Arc::clone(runner),
            ext_index,
            &entry.custom_type,
            &entry_json,
            self.tool_output_expanded,
            self.output_pad,
        );
        if !component.has_content() {
            return;
        }
        if let Some(index) = self.streaming_index {
            self.chat_mut().insert_child(index, Box::new(component));
            self.streaming_index = Some(index + 1);
            for pending in self.pending_tools.values_mut() {
                if pending.chat_index >= index {
                    pending.chat_index += 1;
                }
            }
        } else {
            self.add_to_chat(Box::new(component));
        }
    }

    /// `addMessageToChat` case "custom" (`interactive-mode.ts:4032-4048`):
    /// `display:false` renders nothing; otherwise the FIRST extension
    /// renderer for the type (resolved here, `:4034`) — or no renderer at
    /// all — feeds a `CustomMessageComponent`, whose default box always
    /// shows the message. The row lands at the chat tail: upstream goes
    /// through `addMessageToChat`, not the splicing `addCustomEntryToChat`.
    #[cfg(feature = "ext-js")]
    fn add_custom_message_to_chat(
        &mut self,
        message: &pi_agent::CustomMessage,
        message_id: Option<&str>,
        timestamp: Option<&str>,
    ) {
        if !message.display {
            return;
        }
        let renderer = self.extension_runner.as_ref().and_then(|runner| {
            let locked = runner.lock().expect("extension runner mutex poisoned");
            locked
                .get_message_renderer(&message.custom_type)
                .map(|ext_index| (std::sync::Arc::clone(runner), ext_index))
        });
        let component = super::components::custom_message::CustomMessageComponent::new(
            renderer,
            message,
            self.tool_output_expanded,
            self.output_pad,
            self.markdown_transform_ctx.clone(),
            message_id.map(str::to_string),
            timestamp.map(str::to_string),
        );
        self.add_to_chat(Box::new(component));
    }

    /// `getRegisteredToolDefinition` (`interactive-mode.ts:2356-2360`):
    /// the tool-renderer resolver chain over the extension runner —
    /// resolvers in load order, the Rust built-in availability crossing as
    /// the chain's base. `None` keeps the row on the built-in/generic
    /// paths; a resolver throw takes the same route (upstream propagates
    /// it into row creation, the port falls back).
    #[cfg(feature = "ext-js")]
    fn resolve_tool_renderers(
        &self,
        tool_name: &str,
    ) -> Option<super::components::tool_execution::JsToolRenderers> {
        let runner = self.extension_runner.as_ref()?;
        let has_builtin = crate::tools::renderers::renderers_for(tool_name).is_some();
        let locked = runner.lock().expect("extension runner mutex poisoned");
        match locked.resolve_tool_renderers(tool_name, has_builtin) {
            Ok(pi_ext_js::ToolRenderersOutcome::Renderers(resolved)) => {
                Some(super::components::tool_execution::JsToolRenderers::new(
                    std::sync::Arc::clone(runner),
                    &resolved,
                ))
            }
            _ => None,
        }
    }

    #[cfg(not(feature = "ext-js"))]
    fn resolve_tool_renderers(
        &self,
        tool_name: &str,
    ) -> Option<super::components::tool_execution::JsToolRenderers> {
        let _ = tool_name;
        None
    }

    /// `renderSessionEntries` + `renderSessionItems` (`interactive-mode.ts:3437-3464`,
    /// `:3340-3425`): walk session entries and re-create the chat rows. The
    /// compaction summary and its billed-tokens notice come from the
    /// `compaction` entry; assistant tool calls open tool rows that later
    /// `toolResult` messages complete; rows with no result stay pending, like
    /// upstream's `pendingTools` handoff.
    ///
    /// Deviations: entries with no ported component are skipped — the
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
                            self.output_pad,
                            self.markdown_transform_ctx.clone(),
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
                crate::session::records::SessionEntry::BranchSummary(summary)
                    if !summary.summary.is_empty() =>
                {
                    // `case "branchSummary"` (`interactive-mode.ts:4063-4074`):
                    // a spacer, then the collapsible row.
                    self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
                    let mut row =
                        super::components::branch_summary::BranchSummaryMessageComponent::new(
                            &summary.summary,
                            self.output_pad,
                            self.markdown_transform_ctx.clone(),
                        );
                    row.set_expanded(self.tool_output_expanded);
                    self.add_to_chat(Box::new(row));
                }
                // `renderSessionEntries` keeps `custom` entries whole
                // (`interactive-mode.ts:4270-4272`) and
                // `renderSessionItems` routes them to
                // `addCustomEntryToChat` (`:4171-4173`).
                #[cfg(feature = "ext-js")]
                crate::session::records::SessionEntry::Custom(custom) => {
                    self.add_custom_entry_to_chat(custom);
                }
                // Custom messages go through the generic message path with
                // the persisted meta: `sessionEntryToContextMessages`
                // (`session-manager.ts:453-456`) builds the message, the
                // meta map carries the entry id + ISO timestamp
                // (`interactive-mode.ts:4268-4282`), and `renderSessionItems`
                // passes them to `addMessageToChat` (`:4241-4246`).
                #[cfg(feature = "ext-js")]
                crate::session::records::SessionEntry::CustomMessage(custom) => {
                    for message in crate::session::store::session_entry_to_context_messages(entry) {
                        if let pi_agent::AgentMessage::Custom(custom_message) = message {
                            self.add_custom_message_to_chat(
                                &custom_message,
                                Some(&custom.base.id),
                                Some(&custom.base.timestamp),
                            );
                        }
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
    #[allow(clippy::too_many_lines)] // mirrors the TS replay; split when extending
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
                    self.add_to_chat(Box::new(UserMessageComponent::new(
                        &text,
                        self.output_pad,
                        self.markdown_transform_ctx.clone(),
                    )));
                    if populate_history {
                        self.editor_add_to_history(&text);
                    }
                }
            }
            AgentMessage::Assistant(assistant) => {
                let mut component = AssistantMessageComponent::new(
                    self.hide_thinking_block,
                    &self.hidden_thinking_label,
                    self.output_pad,
                    self.markdown_transform_ctx.clone(),
                );
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
                        self.output_pad,
                        &self.session.get_cwd().to_string_lossy(),
                        crate::tools::renderers::renderers_for(&call.name),
                        self.resolve_tool_renderers(&call.name),
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
            AgentMessage::BranchSummary(summary) => {
                // `case "branchSummary"` (`interactive-mode.ts:4063-4074`).
                self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
                let mut row = super::components::branch_summary::BranchSummaryMessageComponent::new(
                    &summary.summary,
                    self.output_pad,
                    self.markdown_transform_ctx.clone(),
                );
                row.set_expanded(self.tool_output_expanded);
                self.add_to_chat(Box::new(row));
            }
            // System messages render nothing upstream either
            // (`case "system": break`); custom/bash/compaction-summary
            // messages have no ported component.
            _ => {}
        }
    }

    fn start_assistant_stream(&mut self) {
        let component = AssistantMessageComponent::new(
            self.hide_thinking_block,
            &self.hidden_thinking_label,
            self.output_pad,
            self.markdown_transform_ctx.clone(),
        );
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
            if let Some(chat_index) = self
                .pending_tools
                .get(&call.id)
                .map(|pending| pending.chat_index)
            {
                // Streaming partials refresh an open row's args
                // (`interactive-mode.ts:3674-3678`): the first partial
                // carries empty arguments, and without this the call line
                // keeps the empty-command placeholder.
                if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(chat_index) {
                    component.update_args(serde_json::Value::Object(call.arguments.clone()));
                }
                continue;
            }
            let component = ToolExecutionComponent::new(
                &call.name,
                &call.id,
                serde_json::Value::Object(call.arguments.clone()),
                self.output_pad,
                &self.session.get_cwd().to_string_lossy(),
                crate::tools::renderers::renderers_for(&call.name),
                self.resolve_tool_renderers(&call.name),
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
        // `handleEvent`'s preamble (`interactive-mode.ts:3531-3536`): the
        // program status reporter consumes its event subset first. (The
        // paired `footer.invalidate()` is a no-op upstream — `footer.ts:85-87`
        // — and the port's footer recomputes from the session on demand, so
        // nothing to do for it here.)
        let session_name = self.session.get_session_name();
        self.program_status
            .handle_event(event, session_name.as_deref(), &mut self.tui);
        match event {
            AgentEvent::AgentStart
            | AgentEvent::TurnStart
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::ToolExecutionEnd { .. }
            | AgentEvent::AutoRetryStart { .. }
            | AgentEvent::AutoRetryEnd { .. }
            | AgentEvent::CompactionStart { .. }
            | AgentEvent::CompactionEnd { .. }
            | AgentEvent::AgentSettled { .. }
            | AgentEvent::QueueUpdate { .. }
            | AgentEvent::ThinkingLevelChanged { .. } => {
                // The final tool result renders with its message end. The
                // pump drives its runs directly; the retry, compaction,
                // appended-entry usage/compaction displays, settled, and
                // queue/level-change displays land with the pump's
                // AgentRun rework (upstream shows them via agent-session
                // events).
            }
            // `session_info_changed` (`interactive-mode.ts:3614-3618`): the
            // title carries the new session name; the paired
            // `footer.invalidate()` is a no-op upstream (`footer.ts:85-87`).
            AgentEvent::SessionInfoChanged { .. } => {
                self.update_terminal_title();
                self.tui.request_render();
            }
            // `entry_appended` (`interactive-mode.ts:3569-3587`): custom
            // entries splice ahead of the streaming row, custom messages
            // land through `addMessageToChat` (`createCustomMessage` makes
            // the message, `core/messages.ts:123-137`, ISO → epoch ms, no
            // persisted meta for live rows) — the usage notices and the
            // compaction rebuild land with the pump's AgentRun rework.
            #[cfg(feature = "ext-js")]
            AgentEvent::EntryAppended { entry } => {
                let entry_type = entry.get("type").and_then(serde_json::Value::as_str);
                if entry_type == Some("custom")
                    && let Ok(custom) = serde_json::from_value::<crate::session::records::CustomEntry>(
                        entry.clone(),
                    )
                {
                    self.add_custom_entry_to_chat(&custom);
                    self.tui.request_render();
                } else if entry_type == Some("custom_message")
                    && let Ok(custom) = serde_json::from_value::<
                        crate::session::records::CustomMessageEntry,
                    >(entry.clone())
                {
                    let message = pi_agent::CustomMessage {
                        custom_type: custom.custom_type,
                        content: custom.content,
                        display: custom.display,
                        details: custom.details,
                        timestamp: crate::session::store::iso_to_ms(&custom.base.timestamp),
                    };
                    self.add_custom_message_to_chat(&message, None, None);
                    self.tui.request_render();
                }
            }
            #[cfg(not(feature = "ext-js"))]
            AgentEvent::EntryAppended { .. } => {
                // The appended-entry displays land with the pump's AgentRun
                // rework (upstream shows them via agent-session events).
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
            AgentEvent::AgentEnd {
                messages,
                will_retry,
            } => {
                // The pump's post-run machine consumes the verdict and the
                // run's messages (`_handlePostAgentRun`, `agent-session.ts`
                // :2062-2098).
                self.last_agent_end_will_retry = *will_retry;
                self.last_agent_end_messages = Some(messages.clone());
                self.refresh_footer();
                self.tui.request_render();
            }
        }
    }

    /// Recompute the footer's session-derived cells: the usage totals
    /// (`FooterComponent.getSessionStats`), the model and thinking level, the
    /// available provider count, and the routed virtual model.
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
        // The `(provider)` prefix tracks the CURRENT model like `model_id`
        // does — a stale boot provider survived a `/model` switch (the
        // footer's `footerData.model.provider` is rebuilt per refresh,
        // `footer.ts:227-263`).
        let model_provider = self.model.provider.clone();
        let model_supports_reasoning = self.model.reasoning;
        // The provider count behind the `(provider)` prefix
        // (`updateAvailableProviderCount`, `interactive-mode.ts:5444-5451`).
        let provider_count = available_provider_count(&self.auth_path);
        // Under a virtual selection, the physical model of the latest
        // successful response (`session.routedModel`, `agent-session.ts:1538-1543`;
        // `findLatestResponse`, `virtual-models.ts:110-118`; `getPhysicalModel`
        // keeps only non-virtual answers).
        let routed = if crate::virtual_models::is_virtual_model(&self.model) {
            self.session
                .build_session_projection()
                .ok()
                .and_then(|projection| {
                    crate::virtual_models::find_latest_response(&projection.messages).and_then(
                        |latest| {
                            crate::models_state::lookup(&latest.provider, &latest.model)
                                .filter(|physical| {
                                    !crate::virtual_models::is_virtual_model(physical)
                                })
                                .map(|physical| {
                                    (
                                        physical.id.clone(),
                                        latest
                                            .thinking_level
                                            .map(|level| recorded_level_name(level).to_string()),
                                    )
                                })
                        },
                    )
                })
        } else {
            None
        };
        let (routed_model_id, routed_thinking_level) =
            routed.map_or((None, None), |(id, level)| (Some(id), level));
        // `setAutoCompactEnabled(this.session.autoCompactionEnabled)`
        // (`interactive-mode.ts:686`, `:2197`): the footer reads the live
        // gate on every refresh. Read before the tree borrow.
        let auto_compact_enabled = self.settings.get_compaction_enabled();
        let Some(footer) = self.footer_mut() else {
            return;
        };
        footer.data.totals = totals;
        footer.data.latest_cache_hit_rate = latest_cache_hit_rate;
        footer.data.context_percent = context_percent;
        footer.data.thinking_level = thinking_level;
        footer.data.model_id = model_id;
        footer.data.model_provider = model_provider;
        footer.data.model_supports_reasoning = model_supports_reasoning;
        footer.data.available_provider_count = provider_count;
        footer.data.routed_model_id = routed_model_id;
        footer.data.routed_thinking_level = routed_thinking_level;
        footer.data.auto_compact_enabled = auto_compact_enabled;
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

    #[allow(clippy::too_many_lines)] // mirrors the TS submit chain; split when extending
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
        // `/scoped-models` (`interactive-mode.ts:5572-5673`): the
        // enable/disable selector behind ctrl+p cycling.
        if text == "/scoped-models" {
            self.open_scoped_models_selector();
            return;
        }
        // `/reload` (`interactive-mode.ts:6618-6719`): the resource reload.
        if text == "/reload" {
            self.handle_reload_command();
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
        // `/export [path]` (`interactive-mode.ts:3352-3356`).
        if text == "/export" || text.starts_with("/export ") {
            self.handle_export_command(&text);
            return;
        }
        // `/import <path>` (`interactive-mode.ts:3357-3361`).
        if text == "/import" || text.starts_with("/import ") {
            self.handle_import_command(&text);
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
        // `/fork` (`interactive-mode.ts:3398-3401`).
        if text == "/fork" {
            self.show_user_message_selector();
            return;
        }
        // `/clone` (`interactive-mode.ts:3403-3406`): upstream clears the
        // editor before awaiting the fork.
        if text == "/clone" {
            self.editor_mut().editor.set_text("");
            self.handle_clone_command();
            return;
        }
        // `/tree` (`interactive-mode.ts:3408-3411`).
        if text == "/tree" {
            self.show_tree_selector(None);
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
        // Queue input during compaction or a branch summary
        // (`interactive-mode.ts:3489-3499`, `session.isCompacting` covering
        // both — `agent-session.ts:1719-1726`): built-in commands already
        // dispatched above.
        if self.is_compacting() {
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
        self.editor_add_to_history(text);
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
    /// `_prepareRetry` (`agent-session.ts:3938-3975`): budget the attempt,
    /// omit the failed attempt from the projection, and sleep abortably.
    /// The display stands in for `RetryStatusIndicator`'s countdown.
    fn start_retry_wait(&mut self) {
        let settings = self.settings.get_retry_settings();
        self.retry_attempt += 1;
        if self.retry_attempt > settings.max_retries.max(0) as u32 {
            // Preserve the completed attempt count so post-run handling can
            // emit the final failure (:3947-3952).
            self.retry_attempt -= 1;
            return;
        }
        let policy = pi_ai::retry::RetryPolicy {
            enabled: settings.enabled,
            max_retries: settings.max_retries.max(0) as u32,
            base_delay_ms: settings.base_delay_ms.max(0) as u64,
            max_agent_delay_ms: Some(settings.max_agent_delay_ms.max(0) as u64),
        };
        let delay_ms = pi_ai::retry::retry_delay_ms(&policy, self.retry_attempt);
        // `_prepareRetry` omits before the backoff (:3959-3963); the failed
        // attempt is already on the branch (persisted on `MessageEnd`).
        self.omit_persisted_attempt();
        let seconds = delay_ms.div_ceil(1000);
        self.set_working_status(Some(StatusIndicator::retry(&format!(
            "Retrying ({}/{}) in {seconds}s... (esc to cancel)",
            self.retry_attempt,
            settings.max_retries.max(0) as u32
        ))));
        self.tui.request_render();

        let (tx, rx) = std::sync::mpsc::channel::<RetryWaitEvent>();
        let signal = Arc::new(AbortSignal::new());
        let run_signal = Arc::clone(&signal);
        let base_delay_ms = settings.base_delay_ms.max(0) as u64;
        let max_agent_delay_ms = settings.max_agent_delay_ms.max(0) as u64;
        let attempt = self.retry_attempt;
        let max_retries = settings.max_retries.max(0) as u32;
        let handle = std::thread::spawn(move || {
            let policy = pi_ai::retry::RetryPolicy {
                enabled: true,
                max_retries,
                base_delay_ms,
                max_agent_delay_ms: Some(max_agent_delay_ms),
            };
            let delay_ms = pi_ai::retry::retry_delay_ms(&policy, attempt);
            let outcome = if crate::modes::retry::sleep_abortable(delay_ms, &run_signal) {
                RetryWaitEvent::Fire
            } else {
                RetryWaitEvent::Cancelled
            };
            let _ = tx.send(outcome);
        });
        self.retry_wait = Some(RetryWait {
            abort: signal,
            events: rx,
            handle: Some(handle),
        });
    }

    /// `agent.continue()`: the retry or overflow recovery re-runs the same
    /// turn with the retired run's prompts and context.
    fn spawn_retry_run(&mut self) {
        if let Some((prompts, context)) = self.last_run_turn.take() {
            self.spawn_run(prompts, context);
        }
    }

    /// `_checkCompaction` on the retired turn (`agent-session.ts:3119-3252`):
    /// the automatic cases the run tails drive; the pump owns the session,
    /// so the machine decides here and the threaded compaction runs from
    /// `handle_compact_command`.
    fn post_run_compaction(&mut self, aborted: bool) {
        let Some(messages) = self.last_agent_end_messages.clone() else {
            return;
        };
        let Some((_, context)) = self.last_run_turn.clone() else {
            return;
        };
        let mut full_messages = context.messages.clone();
        full_messages.extend(messages.iter().cloned());
        let settings = self
            .settings
            .get_compaction_settings(Some((self.model.provider.as_str(), self.model.id.as_str())))
            .unwrap_or(crate::session::compaction::DEFAULT_COMPACTION_SETTINGS);
        let mut machine = crate::modes::compaction::AutoCompaction::new(settings);
        match machine.post_run_decision(
            aborted,
            &messages,
            &full_messages,
            &self.session,
            &self.model,
        ) {
            crate::modes::compaction::Decision::None => {}
            crate::modes::compaction::Decision::Threshold => {
                self.auto_continue_after_compaction = false;
                self.handle_compact_command(None);
            }
            crate::modes::compaction::Decision::Overflow { will_retry } => {
                if will_retry {
                    // `_omitRecoveryAttempt` before the recovery compaction
                    // (`agent-session.ts:3209-3211`).
                    self.omit_persisted_attempt();
                }
                self.auto_continue_after_compaction = will_retry;
                self.handle_compact_command(None);
            }
            crate::modes::compaction::Decision::RecoveryFailed { message } => {
                self.show_error(&message);
            }
        }
    }

    /// `_omitRecoveryAttempt` (`agent-session.ts:1311-1336`) on the session
    /// side: the failed attempt is already persisted (the pump's `MessageEnd`
    /// handler appends mid-run, like upstream), so the omission finds the
    /// branch's trailing failed assistant and its tool results and appends
    /// the `context_edit` records. Upstream emits `entry_appended` for each
    /// edit; the interactive UI renders nothing for context edits.
    fn omit_persisted_attempt(&mut self) {
        let Ok(branch) = self.session.get_branch(None) else {
            return;
        };
        let Some(index) = branch.iter().rposition(|entry| {
            matches!(
                entry,
                crate::session::records::SessionEntry::Message(message)
                    if matches!(message.message, AgentMessage::Assistant(_))
            )
        }) else {
            return;
        };
        let tool_call_ids: std::collections::HashSet<String> = match &branch[index] {
            crate::session::records::SessionEntry::Message(message) => match &message.message {
                AgentMessage::Assistant(assistant) => assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        pi_ai::types::AssistantContent::ToolCall(call) => Some(call.id.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => return,
            },
            _ => return,
        };
        let mut target_ids = vec![branch[index].id().to_string()];
        for entry in &branch[index + 1..] {
            let crate::session::records::SessionEntry::Message(message) = entry else {
                continue;
            };
            match &message.message {
                AgentMessage::Assistant(_) => break,
                AgentMessage::ToolResult(result)
                    if tool_call_ids.contains(&result.tool_call_id) =>
                {
                    target_ids.push(entry.id().to_string());
                }
                _ => {}
            }
        }
        for id in target_ids {
            let _ = self
                .session
                .append_context_edit(&id, &crate::session::records::ContextReplacement::Omit);
        }
    }

    /// `compaction_end` handling.
    #[allow(clippy::too_many_lines)] // mirrors the TS compaction flow incl. virtual routing; split when extending
    fn handle_compact_command(&mut self, custom_instructions: Option<&str>) {
        if self.compaction_run.is_some() {
            return;
        }
        self.abort_active_run();

        let settings = self
            .settings
            .get_compaction_settings(Some((self.model.provider.as_str(), self.model.id.as_str())))
            .unwrap_or(crate::session::compaction::DEFAULT_COMPACTION_SETTINGS);

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
                        signal: None,
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
                let (api_key, access_token) = self.run_wire_credential();
                (model, api_key, access_token, self.thinking_level)
            };
        #[cfg(not(feature = "ext-js"))]
        let (model, api_key, access_token, thinking_level) = {
            let (api_key, access_token) = self.run_wire_credential();
            (model, api_key, access_token, self.thinking_level)
        };
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
        // `_finishCancelledRetry` (`agent-session.ts:3922-3932`): an abort
        // during a run or its wait cancels the pending retry.
        if self.retry_attempt > 0 {
            let attempt = self.retry_attempt;
            self.retry_attempt = 0;
            self.show_error(&format!(
                "Retry failed after {attempt} attempts: Retry cancelled"
            ));
        }
        self.last_run_turn = None;
        self.set_working_status(None);
    }

    /// `compaction_end` handling (`interactive-mode.ts:3820-3855`): persist
    /// the summary, render the rows, flush the queue.
    #[allow(clippy::too_many_lines)] // mirrors the TS compaction flow; split when extending
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
        // `_runAutoCompaction`'s `willRetry` → `agent.continue()`:
        // the overflow recovery re-runs the turn once the compaction lands.
        if self.auto_continue_after_compaction {
            self.auto_continue_after_compaction = false;
            self.spawn_retry_run();
        }
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
            self.output_pad,
            self.markdown_transform_ctx.clone(),
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
        self.editor_add_to_history(text);
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
            crate::models_state::set_extension_runner(std::sync::Arc::clone(runner));
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
        self.update_editor_border_color();
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

    /// `cycleThinkingLevel` (`interactive-mode.ts:4711-4721`) over the
    /// model's supported levels (`agent-session.ts:2813-2824`): modulo the
    /// current index; an unlisted current level restarts at the first
    /// (upstream's `indexOf` → `-1` → `0`).
    fn cycle_thinking_level(&mut self) {
        if !self.model.reasoning {
            self.show_status("Current model does not support thinking");
            return;
        }
        let levels = pi_ai::get_supported_thinking_levels(&self.model);
        let next_index = levels
            .iter()
            .position(|level| *level == self.thinking_level.as_str())
            .map_or(0, |index| (index + 1) % levels.len());
        let Some(next_level) = levels
            .get(next_index)
            .and_then(|level| crate::modes::print_mode::thinking_level_from_str(level).ok())
        else {
            return;
        };
        self.thinking_level = next_level;
        let _ = self
            .session
            .append_thinking_level_change(next_level.as_str());
        self.refresh_footer();
        self.update_editor_border_color();
        self.show_status(&format!("Thinking level: {}", next_level.as_str()));
    }

    /// `updateEditorBorderColor` (`interactive-mode.ts:4700-4707`): bash
    /// mode takes the `bashMode` color; otherwise the border carries the
    /// thinking level's color.
    fn update_editor_border_color(&mut self) {
        // `this.editor` stays reachable while a selector occupies the slot —
        // the settings dialog's arms fire mid-dialog with the editor parked,
        // so the parked host counts here (`editor_host_mut`).
        let is_bash_mode = self
            .editor_host_mut()
            .is_some_and(|editor| editor.is_bash_mode);
        let styler = if is_bash_mode {
            theme::bash_mode_border_styler()
        } else {
            theme::thinking_border_styler(self.thinking_level)
        };
        if let Some(editor) = self.editor_host_mut() {
            editor.editor.set_border_color(styler);
        }
        self.tui.request_render();
    }

    /// ctrl+p / ctrl+shift+p cycling (`cycleModel`, `interactive-mode.ts:4722-4741`).
    /// The session scope wins over the whole available snapshot
    /// (`agent-session.ts:2699-2707`); upstream also applies per-model
    /// thinking defaults on the switch, which land with the settings work.
    fn cycle_model(&mut self, direction: i32) {
        if !self.scoped_models.is_empty() {
            self.cycle_scoped_model(direction);
            return;
        }
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
        self.report_model_switch();
    }

    /// `_cycleScopedModel` (`agent-session.ts:2709-2744`): the scope
    /// filters to still-available models before cycling; a pattern's
    /// explicit thinking level applies on the switch.
    fn cycle_scoped_model(&mut self, direction: i32) {
        let available = self.available_models();
        let available_ids: std::collections::HashSet<String> = available
            .iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect();
        let scoped: Vec<&ScopedModel> = self
            .scoped_models
            .iter()
            .filter(|scoped| {
                available_ids.contains(&format!("{}/{}", scoped.model.provider, scoped.model.id))
            })
            .collect();
        let current_index = scoped
            .iter()
            .position(|scoped| models_are_equal(&scoped.model, &self.model))
            .map_or(-1, |index| index as isize);
        let Some(next_index) = next_cycle_index(current_index, scoped.len(), direction > 0) else {
            // `Only one model in scope` (`interactive-mode.ts:4726`).
            self.show_status("Only one model in scope");
            return;
        };
        let next_model = scoped[next_index].model.clone();
        let next_thinking_level = scoped[next_index].thinking_level;
        // The explicit pattern level wins; then the default-thinking-level
        // setting; then the current level (`_getThinkingLevelForModelSwitch`,
        // `agent-session.ts:2841-2853` — the port has no per-model overrides
        // yet). Upstream clamps to the model's capabilities
        // (`setThinkingLevel`).
        let level = next_thinking_level.unwrap_or_else(|| {
            self.settings
                .get_default_thinking_level()
                .and_then(|level| crate::modes::print_mode::thinking_level_from_str(&level).ok())
                .unwrap_or(self.thinking_level)
        });
        if let Err(error) = self.apply_model(next_model) {
            self.show_error(&error);
            return;
        }
        self.thinking_level = level;
        self.report_model_switch();
    }

    /// The `Switched to ${model.name || model.id}` + thinking-suffix status
    /// (`interactive-mode.ts:4731-4735`).
    fn report_model_switch(&mut self) {
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
        // `updateEditorBorderColor` on the switch
        // (`interactive-mode.ts:4728-4730`).
        self.update_editor_border_color();
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

    /// The session scope's `provider/id` list
    /// (`sessionScopedModels.map(...)`, `interactive-mode.ts:5587`).
    fn scoped_ids(&self) -> Vec<String> {
        self.scoped_models
            .iter()
            .map(|scoped| format!("{}/{}", scoped.model.provider, scoped.model.id))
            .collect()
    }

    /// `configuredEnabledIds` (`interactive-mode.ts:5575-5583`): the
    /// `enabledModels` patterns resolved to ids over the given snapshot,
    /// with each unmatched pattern appended so the selector shows it
    /// unchecked. `None` when no patterns are configured (= all enabled).
    fn configured_enabled_ids(
        patterns: Option<&[String]>,
        models: &[pi_ai::types::Model],
    ) -> Option<Vec<String>> {
        let patterns = patterns?;
        if patterns.is_empty() {
            return None;
        }
        let resolved = resolve_model_scope_from_models(patterns, models);
        let mut ids: Vec<String> = resolved
            .scoped_models
            .iter()
            .map(|scoped| format!("{}/{}", scoped.model.provider, scoped.model.id))
            .collect();
        for diagnostic in &resolved.diagnostics {
            if diagnostic.code == ModelScopeDiagnosticCode::NoMatch
                && !ids.contains(&diagnostic.pattern)
            {
                ids.push(diagnostic.pattern.clone());
            }
        }
        Some(ids)
    }

    /// The mounted `/scoped-models` selector, downcast out of the
    /// suspended-editor slot (the component's `as_any_mut` override).
    fn scoped_selector_mut(&mut self) -> Option<&mut ScopedModelsSelectorComponent> {
        let slot = self.suspended_editor.as_mut()?;
        slot.as_any_mut()?
            .downcast_mut::<ScopedModelsSelectorComponent>()
    }

    /// `updateSessionModels` (`interactive-mode.ts:5591-5609`): apply the
    /// enabled set to the session scope — scoped only when at least one
    /// enabled id is available and not every available model is enabled;
    /// any other combination resets to the unscoped catalog — then refresh
    /// the footer and render.
    fn update_session_models(&mut self, enabled_ids: Option<&[String]>) {
        let available = self.available_models();
        let available_ids: Vec<String> = available
            .iter()
            .map(|model| format!("{}/{}", model.provider, model.id))
            .collect();
        let scoped = match enabled_ids {
            Some(ids) => {
                let has_enabled_available = ids.iter().any(|id| available_ids.contains(id));
                let all_available_enabled = available_ids.iter().all(|id| ids.contains(id));
                if has_enabled_available && !all_available_enabled {
                    resolve_model_scope_from_models(ids, &available).scoped_models
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        };
        self.set_scoped_models(scoped);
        self.refresh_footer();
        self.tui.request_render();
    }

    /// Mount the `/scoped-models` selector in the editor's tree slot and
    /// run the catalog refresh over it (`showModelsSelector`,
    /// `interactive-mode.ts:5570-5686`). Upstream refreshes asynchronously:
    /// the selector mounts with the cached snapshot and "Refreshing model
    /// catalogs…", then the network pass swaps the fresh snapshot in. The
    /// port's compose is synchronous ([`super::model_catalog_refresh`]), so
    /// the same sequence runs inline — mount, refresh, post-refresh tail —
    /// and the timed-out arm has no synchronous counterpart.
    #[allow(clippy::too_many_lines)] // mirrors the TS showModelsSelector; split when extending
    fn open_scoped_models_selector(&mut self) {
        if self.suspended_editor.is_some() {
            return;
        }
        let available = self.available_models();
        let configured_patterns = self.settings.get_enabled_models();
        // The entry state (`interactive-mode.ts:5585-5588`): the session
        // scope's ids when one is set, else the configured patterns
        // resolved over the snapshot.
        let mut current_enabled_ids = if self.scoped_models.is_empty() {
            Self::configured_enabled_ids(configured_patterns.as_deref(), &available)
        } else {
            Some(self.scoped_ids())
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = ScopedModelsSelectorComponent::new(
            ModelsConfig {
                all_models: available.clone(),
                enabled_model_ids: current_enabled_ids.clone(),
                refresh_status: Some("Refreshing model catalogs…".to_string()),
            },
            Arc::clone(&events),
        );
        self.scoped_models_selector_events = Some(events);
        self.suspend_editor_with(Box::new(selector));

        // The refresh tail (`interactive-mode.ts:5645-5667`), synchronous:
        // nothing has been pumped yet, so `selectionChanged` is false and
        // the session scope still holds its entry value — the configured
        // branch re-resolves over the fresh snapshot, the session branch
        // keeps the selector's enabled set.
        #[cfg(feature = "ext-js")]
        let result = refresh_model_catalogs(self.extension_runner.as_ref());
        #[cfg(not(feature = "ext-js"))]
        let result = refresh_model_catalogs();
        let available = self.available_models();
        let enabled_for_selector: Option<Option<Vec<String>>> = if self.scoped_models.is_empty() {
            let fresh = Self::configured_enabled_ids(configured_patterns.as_deref(), &available);
            current_enabled_ids.clone_from(&fresh);
            Some(fresh)
        } else {
            None
        };
        if let Some(selector) = self.scoped_selector_mut() {
            match enabled_for_selector {
                Some(enabled) => selector.update_models(&available, Some(enabled)),
                None => selector.update_models(&available, None),
            }
        }
        if let Some(ids) = &current_enabled_ids {
            self.update_session_models(Some(ids));
        }
        // Errors name their providers (`interactive-mode.ts:5659-5665`).
        let status: (String, RefreshStatusKind) = if result.errors.is_empty() {
            (
                "Model catalogs refreshed.".to_string(),
                RefreshStatusKind::Success,
            )
        } else {
            let providers = result.errors.keys().cloned().collect::<Vec<_>>().join(", ");
            (
                format!("Could not refresh {providers}; showing cached models."),
                RefreshStatusKind::Warning,
            )
        };
        if let Some(selector) = self.scoped_selector_mut() {
            selector.set_refresh_status(&status.0, status.1);
        }
        self.tui.request_render();
    }

    /// Put the editor back (`showSelector`'s `done`).
    fn close_scoped_models_selector(&mut self) {
        if self.suspended_editor.is_none() {
            return;
        }
        self.scoped_models_selector_events = None;
        self.restore_editor();
    }

    /// The `/scoped-models` selector's drain (`showModelsSelector`'s
    /// onChange / onPersist / onCancel, `interactive-mode.ts:5626-5642`).
    fn pump_scoped_models_selector(&mut self) {
        if self.scoped_models_selector_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .scoped_models_selector_events
                .as_ref()
                .expect("scoped models selector")
                .lock()
                .expect("selector events"),
        );
        for event in events {
            match event {
                ScopedModelsSelectorEvent::Change(enabled_ids) => {
                    self.update_session_models(enabled_ids.as_deref());
                }
                ScopedModelsSelectorEvent::Persist(enabled_ids) => {
                    // `onPersist` (`interactive-mode.ts:5630-5638`): the
                    // patterns clear when nothing is selected or the
                    // selection covers every available model.
                    let available = self.available_models();
                    let available_ids: Vec<String> = available
                        .iter()
                        .map(|model| format!("{}/{}", model.provider, model.id))
                        .collect();
                    let all_enabled = match &enabled_ids {
                        None => false,
                        Some(ids) => {
                            ids.len() == available.len()
                                && ids.iter().all(|id| available_ids.contains(id))
                        }
                    };
                    let new_patterns = match (&enabled_ids, all_enabled) {
                        (None, _) | (Some(_), true) => None,
                        (Some(ids), false) => Some(ids.clone()),
                    };
                    self.settings.set_enabled_models(new_patterns);
                    self.show_status("Model selection saved to settings");
                }
                ScopedModelsSelectorEvent::Cancel => {
                    self.close_scoped_models_selector();
                    self.tui.request_render();
                }
            }
        }
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
        // One guard for both queue modes: two `self.queues.lock()` calls in
        // one statement deadlock — the first MutexGuard is a temporary of
        // the whole `let`, still held when the second call runs (the
        // theme() pattern from 2026-11-14).
        let (steering_mode, follow_up_mode) = {
            let queues = self.queues.lock().expect("queues");
            (
                queue_mode_as_setting(queues.steering_mode()).to_string(),
                queue_mode_as_setting(queues.follow_up_mode()).to_string(),
            )
        };
        let selector = SettingsSelectorComponent::new(
            &SettingsSelectorConfig {
                // `autoCompact` = `session.autoCompactionEnabled`
                // (`interactive-mode.ts:5105` = `getCompactionEnabled`).
                auto_compact: self.settings.get_compaction_enabled(),
                steering_mode,
                follow_up_mode,
                transport: self.settings.get_transport(),
                // An invalid stored timeout throws upstream, which would
                // abort opening /settings; the port degrades to the default.
                // deviation: defensive read.
                http_idle_timeout_ms: self
                    .settings
                    .get_http_idle_timeout_ms()
                    .unwrap_or(crate::config::settings::DEFAULT_HTTP_IDLE_TIMEOUT_MS),
                cache_warming_mode: self.settings.get_cache_warming_mode(),
                hide_thinking_block: self.hide_thinking_block,
                mermaid_rendering_mode: self
                    .settings
                    .get_mermaid_rendering_mode()
                    .as_name()
                    .to_string(),
                show_cache_miss_notices: self.settings.get_show_cache_miss_notices(),
                collapse_changelog: self.settings.get_collapse_changelog(),
                enable_install_telemetry: self.settings.get_enable_install_telemetry(),
                quiet_startup: match self.settings.get_quiet_startup() {
                    crate::config::settings::QuietStartup::Quiet => "true",
                    crate::config::settings::QuietStartup::Header => "header",
                    crate::config::settings::QuietStartup::Off => "false",
                }
                .to_string(),
                // `getDefaultProjectTrust` reads the global layer only
                // (`settings-manager.ts:1020-1023`).
                default_project_trust: self
                    .settings
                    .get_default_project_trust()
                    .as_name()
                    .to_string(),
                double_escape_action: self.settings.get_double_escape_action(),
                tree_filter_mode: self.settings.get_tree_filter_mode().as_name().to_string(),
                thinking_level: self
                    .settings
                    .get_default_thinking_level()
                    .unwrap_or_else(|| "off".to_string()),
                model_thinking_levels: model_thinking_levels(&self.settings),
                available_models: self.available_models(),
                current_model: Some((self.model.provider.clone(), self.model.id.clone())),
                default_model,
                show_images: self.settings.get_show_images(),
                image_width_cells: self.settings.get_image_width_cells(),
                auto_resize_images: self.settings.get_image_auto_resize(),
                block_images: self.settings.get_block_images(),
                enable_skill_commands: self.settings.get_enable_skill_commands(),
                show_hardware_cursor: self.settings.get_show_hardware_cursor(),
                editor_padding_x: self.settings.get_editor_padding_x(),
                output_pad: self.settings.get_output_pad(),
                autocomplete_max_visible: self.settings.get_autocomplete_max_visible(),
                clear_on_shrink: self.settings.get_clear_on_shrink(),
                show_terminal_progress: self.settings.get_show_terminal_progress(),
                current_theme: self
                    .theme_setting
                    .clone()
                    .unwrap_or_else(|| pi_tui::theme::SYSTEM_THEME_NAME.to_string()),
                terminal_theme: theme::terminal_theme().to_string(),
                warnings: self.settings.get_warnings(),
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
    /// (`showSettingsSelector`'s callbacks, `interactive-mode.ts:5148-5331`).
    #[allow(clippy::too_many_lines)] // mirrors the TS callback object; split when extending
    fn handle_settings_event(&mut self, event: SettingsSelectorEvent) {
        match event {
            SettingsSelectorEvent::TuiModeChange(_)
            | SettingsSelectorEvent::FullscreenExitOutputChange(_)
            | SettingsSelectorEvent::FullscreenScrollbarChange(_)
            | SettingsSelectorEvent::FullscreenCopyOnSelectChange(_)
            | SettingsSelectorEvent::FullscreenWheelScrollLinesChange(_) => {
                self.handle_fullscreen_settings_event(event);
            }
            SettingsSelectorEvent::AutoCompactChange(enabled) => {
                // `onAutoCompactChange` (`interactive-mode.ts:5149-5152`):
                // `session.setAutoCompactionEnabled` =
                // `settingsManager.setCompactionEnabled`
                // (`agent-session.ts:3417-3419`), then the footer indicator.
                // No border update — upstream's border refresh belongs to the
                // per-model thinking arms (`:5208-5219`) only.
                self.settings.set_compaction_enabled(enabled);
                if let Some(footer) = self.footer_mut() {
                    footer.data.auto_compact_enabled = enabled;
                }
                self.tui.request_render();
            }
            SettingsSelectorEvent::ShowImagesChange(enabled) => {
                // `onShowImagesChange` (`interactive-mode.ts:5153-5160`):
                // persist; the live `ToolExecutionComponent.setShowImages`
                // sweep lands with the image subsystem.
                self.settings.set_show_images(enabled);
            }
            SettingsSelectorEvent::ImageWidthCellsChange(width) => {
                // `onImageWidthCellsChange` (`interactive-mode.ts:5161-5168`).
                self.settings.set_image_width_cells(width);
            }
            SettingsSelectorEvent::AutoResizeImagesChange(enabled) => {
                // `onAutoResizeImagesChange` (`interactive-mode.ts:5169-5171`).
                self.settings.set_image_auto_resize(enabled);
            }
            SettingsSelectorEvent::BlockImagesChange(blocked) => {
                // `onBlockImagesChange` (`interactive-mode.ts:5172-5174`).
                self.settings.set_block_images(blocked);
            }
            SettingsSelectorEvent::EnableSkillCommandsChange(enabled) => {
                // `onEnableSkillCommandsChange`
                // (`interactive-mode.ts:5175-5178`): persist; the
                // autocomplete provider rebuild lands with the skill-command
                // subsystem.
                self.settings.set_enable_skill_commands(enabled);
            }
            SettingsSelectorEvent::ShowHardwareCursorChange(enabled) => {
                // `onShowHardwareCursorChange`
                // (`interactive-mode.ts:5259-5262`): persist, then push to
                // the live TUI.
                self.settings.set_show_hardware_cursor(enabled);
                self.tui.set_show_hardware_cursor(enabled);
            }
            SettingsSelectorEvent::EditorPaddingXChange(padding) => {
                // `onEditorPaddingXChange` (`interactive-mode.ts:5263-5269`):
                // persist, then push to the editor (mounted or parked).
                self.settings.set_editor_padding_x(padding);
                if let Some(host) = self.editor_host_mut() {
                    host.editor
                        .set_padding_x(usize::try_from(padding).unwrap_or_default());
                }
            }
            SettingsSelectorEvent::OutputPadChange(padding) => {
                // `onOutputPadChange` (`interactive-mode.ts:5270-5281`):
                // persist, remember for new rows, and update every chat row
                // that has the setter.
                self.settings.set_output_pad(padding);
                self.output_pad = usize::try_from(padding).unwrap_or(1);
                let output_pad = self.output_pad;
                let chat_len = self.chat_len();
                for index in 0..chat_len {
                    if let Some(component) = self.downcast_chat::<AssistantMessageComponent>(index)
                    {
                        component.set_output_pad(output_pad);
                    }
                    if let Some(component) = self.downcast_chat::<UserMessageComponent>(index) {
                        component.set_output_pad(output_pad);
                    }
                    if let Some(component) = self.downcast_chat::<ToolExecutionComponent>(index) {
                        component.set_output_pad(output_pad);
                    }
                    if let Some(component) = self.downcast_chat::<
                        super::components::compaction_summary::CompactionSummaryMessageComponent,
                    >(index)
                    {
                        component.set_output_pad(output_pad);
                    }
                    if let Some(component) = self.downcast_chat::<
                        super::components::branch_summary::BranchSummaryMessageComponent,
                    >(index)
                    {
                        component.set_output_pad(output_pad);
                    }
                    // `onOutputPadChange` updates every row with a
                    // `setOutputPad` (`interactive-mode.ts:5270-5281`) —
                    // the custom rows both have one.
                    #[cfg(feature = "ext-js")]
                    if let Some(component) = self
                        .downcast_chat::<super::components::custom_entry::CustomEntryComponent>(
                        index,
                    ) {
                        component.set_output_pad(output_pad);
                    }
                    #[cfg(feature = "ext-js")]
                    if let Some(component) = self.downcast_chat::<
                        super::components::custom_message::CustomMessageComponent,
                    >(index)
                    {
                        component.set_output_pad(output_pad);
                    }
                }
                self.tui.request_render();
            }
            SettingsSelectorEvent::AutocompleteMaxVisibleChange(max_visible) => {
                // `onAutocompleteMaxVisibleChange`
                // (`interactive-mode.ts:5282-5288`): persist, then push to
                // the editor.
                self.settings.set_autocomplete_max_visible(max_visible);
                if let Some(host) = self.editor_host_mut() {
                    host.editor
                        .set_autocomplete_max_visible(usize::try_from(max_visible).unwrap_or(5));
                }
            }
            SettingsSelectorEvent::ClearOnShrinkChange(enabled) => {
                // `onClearOnShrinkChange` (`interactive-mode.ts:5289-5295`):
                // persist, then push to the live TUI. The port's status
                // lives in the chat, so upstream's status-container clear
                // has no counterpart.
                self.settings.set_clear_on_shrink(enabled);
                self.tui.set_clear_on_shrink(enabled);
            }
            SettingsSelectorEvent::ShowTerminalProgressChange(enabled) => {
                // `onShowTerminalProgressChange`
                // (`interactive-mode.ts:5296-5298`); the OSC 9;4 writer is
                // not ported yet.
                self.settings.set_show_terminal_progress(enabled);
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
            SettingsSelectorEvent::TransportChange(transport) => {
                // `onTransportChange` (`interactive-mode.ts:5185-5188`):
                // persist; the live agent transport swap lands with the
                // transport subsystem.
                self.settings.set_transport(&transport);
            }
            SettingsSelectorEvent::HttpIdleTimeoutMsChange(timeout_ms) => {
                // `onHttpIdleTimeoutMsChange`
                // (`interactive-mode.ts:5192-5196`): persist, then report;
                // the dispatcher reconfiguration lands with the HTTP
                // dispatcher subsystem.
                let _ = self.settings.set_http_idle_timeout_ms(timeout_ms);
                self.show_status(&format!(
                    "HTTP idle timeout: {}",
                    format_http_idle_timeout_ms(timeout_ms)
                ));
            }
            SettingsSelectorEvent::CacheWarmingModeChange(mode) => {
                // `onCacheWarmingModeChange`
                // (`interactive-mode.ts:5197-5200`): persist; the warmer
                // restart lands with the cache-warming subsystem.
                self.settings.set_cache_warming_mode(&mode);
                self.show_status(&format!("Cache warming: {mode}"));
            }
            SettingsSelectorEvent::HideThinkingBlockChange(hidden) => {
                // `onHideThinkingBlockChange`
                // (`interactive-mode.ts:5227-5231`).
                self.hide_thinking_block = hidden;
                self.settings
                    .set("hideThinkingBlock", serde_json::Value::Bool(hidden));
                self.update_thinking_block_visibility();
                self.tui.request_render();
            }
            SettingsSelectorEvent::MermaidRenderingModeChange(mode) => {
                // `onMermaidRenderingModeChange`
                // (`interactive-mode.ts:5232-5236`): persist, then repaint;
                // the mermaid renderer lands with its subsystem.
                self.settings.set_mermaid_rendering_mode(mode);
                self.tui.request_render();
            }
            SettingsSelectorEvent::ShowCacheMissNoticesChange(shown) => {
                // `onShowCacheMissNoticesChange`
                // (`interactive-mode.ts:5237-5240`): persist; the chat
                // rebuild lands with the cache-notice subsystem.
                self.settings.set_show_cache_miss_notices(shown);
            }
            SettingsSelectorEvent::CollapseChangelogChange(collapsed) => {
                // `onCollapseChangelogChange`
                // (`interactive-mode.ts:5241-5243`).
                self.settings.set_collapse_changelog(collapsed);
            }
            SettingsSelectorEvent::QuietStartupChange(quiet) => {
                // `onQuietStartupChange` (`interactive-mode.ts:5247-5249`).
                self.settings.set_quiet_startup(quiet);
            }
            SettingsSelectorEvent::EnableInstallTelemetryChange(enabled) => {
                // `onEnableInstallTelemetryChange`
                // (`interactive-mode.ts:5244-5246`); the ping is not ported
                // yet.
                self.settings.set_enable_install_telemetry(enabled);
            }
            SettingsSelectorEvent::DefaultProjectTrustChange(value) => {
                // `onDefaultProjectTrustChange`
                // (`interactive-mode.ts:5250-5252`).
                self.settings
                    .set("defaultProjectTrust", serde_json::Value::String(value));
            }
            SettingsSelectorEvent::DoubleEscapeActionChange(action) => {
                // `onDoubleEscapeActionChange`
                // (`interactive-mode.ts:5253-5255`); the double-escape
                // handler lands with its subsystem.
                self.settings.set_double_escape_action(&action);
            }
            SettingsSelectorEvent::TreeFilterModeChange(mode) => {
                // `onTreeFilterModeChange` (`interactive-mode.ts:5256-5258`).
                if let Some(mode) = crate::config::settings::TreeFilterMode::from_name(&mode) {
                    self.settings.set_tree_filter_mode(mode);
                }
            }
            SettingsSelectorEvent::WarningsChange(warnings) => {
                // `onWarningsChange` (`interactive-mode.ts:5324-5326`).
                self.settings.set_warnings(&warnings);
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
            SettingsSelectorEvent::ThemeChange(setting) => {
                // `onThemeChange` (`interactive-mode.ts:5222-5225`):
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
                self.update_editor_border_color();
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
                self.update_editor_border_color();
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
        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
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
        self.settings.get_cache_warming_mode()
    }

    /// `/hotkeys` (`handleHotkeysCommand`, `interactive-mode.ts:6999-7112`):
    /// every keyboard shortcut, bordered, as markdown.
    fn handle_hotkeys_command(&mut self) {
        // Extension-registered shortcuts (`interactive-mode.ts:7092-7106`):
        // the display key capitalizes like upstream's `formatKeyText(…,
        // { capitalize: true })`, the description defaults to the
        // extension path.
        #[cfg(feature = "ext-js")]
        let extension_shortcuts = self.extension_shortcut_rows();
        #[cfg(not(feature = "ext-js"))]
        let extension_shortcuts: Vec<(String, String)> = Vec::new();
        let hotkeys = hotkeys_markdown(&extension_shortcuts);
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
        // The `changelog` surface (`interactive-mode.ts:2374`): the
        // transformer runs per render, never streaming.
        let transform = self
            .markdown_transform_ctx
            .make("changelog", None, None, None);
        self.add_to_chat(Box::new(pi_tui::components::markdown::Markdown::new(
            changelog_markdown,
            1,
            1,
            theme::markdown_theme(),
            None,
            pi_tui::components::markdown::MarkdownOptions {
                transform,
                ..pi_tui::components::markdown::MarkdownOptions::default()
            },
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
        self.update_editor_border_color();
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
                self.last_status_pair = None;
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
            self.output_pad,
            &self.session.get_cwd().to_string_lossy(),
            crate::tools::renderers::renderers_for("bash"),
            None,
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
            // The MCP extension closes every connection on session_shutdown
            // (`index.ts:1250-1256`); a later `session_start` reconnects.
            if let Ok(mut runner) = runner.lock() {
                runner.shutdown_mcp();
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
        self.editor_add_to_history(text);

        // Session context from the persisted entries (before the new prompt).
        let leaf_id = self.session.get_leaf_id().map(str::to_string);
        let session_context = crate::session::store::build_session_context_from(
            &self.session.get_entries(),
            leaf_id.as_deref(),
        );
        let mut context_messages = session_context.messages;
        if context_messages.is_empty()
            && let Some(system) = pi_agent::transcript::create_initial_system_message(
                Some(&self.system_prompt),
                &self
                    .executor
                    .tools()
                    .iter()
                    .map(tool_declaration)
                    .collect::<Vec<_>>(),
            )
        {
            context_messages.push(AgentMessage::System(system));
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

        self.spawn_run(vec![user_message], context);
    }

    /// The turn's run thread: one `run_agent_loop` pass; the retry and
    /// overflow recovery re-runs happen on the UI thread's post-run machine
    /// (`_handlePostAgentRun`), which re-enters here with the same turn.
    /// The run's wire credential for the CURRENT provider: the stored/env
    /// ladder (`resolve_wire_credential`), with the `--api-key` flag as the
    /// runtime override of the provider it was registered for
    /// (`main.ts:898-906`; request-time `overrides.get(providerId)`,
    /// `runtime-credentials.ts:14-16`). The boot pair was frozen at
    /// construction — a `/model` switch to another provider kept streaming
    /// with the old credential. A virtual selection keeps the frozen pair:
    /// its routing resolves the routed provider's own credential.
    fn run_wire_credential(&self) -> (Option<String>, Option<String>) {
        if crate::virtual_models::is_virtual_model(&self.model) {
            return (self.api_key.clone(), self.access_token.clone());
        }
        let flag_key = crate::oauth::runtime_api_key(&self.model.provider);
        crate::modes::print_mode::resolve_wire_credential(
            &self.model.provider,
            flag_key.as_deref(),
            &self.auth_path,
        )
        .map_or((None, None), |(credential, _)| match credential {
            crate::modes::print_mode::WireCredential::ApiKey(key) => (Some(key), None),
            crate::modes::print_mode::WireCredential::AccessToken(token) => (None, Some(token)),
        })
    }

    fn spawn_run(&mut self, prompts: Vec<AgentMessage>, context: AgentContext) {
        #[cfg(feature = "ext-js")]
        let routing = crate::virtual_models::TurnRouting::install(
            &self.model,
            self.extension_runner.as_ref(),
            self.api_key.clone(),
            &self.session,
        );
        let (api_key, access_token) = self.run_wire_credential();
        let config = AgentLoopConfig {
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            live_settings: None,
            session_id: None,
            api_key,
            access_token,
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
        let retry_attempt = self.retry_attempt;
        let retry_settings = self.settings.get_retry_settings();
        let thread_prompts = prompts.clone();
        let thread_context = context.clone();
        let handle = std::thread::spawn(move || {
            let mut inner_sink = ChannelSink { tx: tx.clone() };
            let mut sink = crate::modes::retry::RetryAwareSink {
                inner: &mut inner_sink,
                retry_attempt,
                settings: retry_settings,
                signal: Arc::clone(&run_signal),
            };
            let _messages = crate::runtime::block_on(pi_agent::run_agent_loop(
                thread_prompts,
                thread_context,
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
            prompts,
            context,
        });
        // `turn_start`'s working gate (`interactive-mode.ts:3553-3560`):
        // the indicator shows only while `workingVisible`, and an existing
        // working indicator is kept — its message rides the extension
        // state (`setWorkingMessage`).
        let active_working = self
            .status_handle
            .lock()
            .expect("status")
            .as_ref()
            .is_some_and(|indicator| indicator.kind == StatusKind::Working);
        if !self.working_visible {
            self.set_working_status(None);
        } else if !active_working {
            let indicator = self.build_working_status_indicator();
            self.set_working_status(Some(indicator));
        }
        self.tui.request_render();
    }

    fn set_working_status(&mut self, indicator: Option<StatusIndicator>) {
        // The editor's border decorator reads the same slot every frame.
        *self.status_handle.lock().expect("status") = indicator;
        // The delegate never embeds the status (E3 slice 9) — the row in
        // the status host tracks the indicator's presence.
        #[cfg(feature = "ext-js")]
        self.sync_extension_editor_status_row();
    }

    /// `showStatus` (`interactive-mode.ts:3964-3984`): a muted line in the
    /// chat tail — `Spacer(1)` + the message. Back-to-back statuses
    /// (nothing else added to the chat in between) REPLACE the previous
    /// pair instead of stacking rows, or thinking-level cycling appends one
    /// line per press. Upstream checks the object identity of the last two
    /// children and sets the text in place; the port compares the rows' heap
    /// addresses and swaps the pair for a fresh one (pi-tui's `as_any_mut`
    /// is deliberately closed, so there is no `Text` downcast) — same rows,
    /// same position, same count.
    fn show_status(&mut self, message: &str) {
        let len = self.chat_len();
        if len >= 2
            && let Some((spacer_addr, text_addr)) = self.last_status_pair
            && self.chat_row_addr(len - 1) == Some(text_addr)
            && self.chat_row_addr(len - 2) == Some(spacer_addr)
        {
            self.chat_mut().remove_child(len - 2);
            self.chat_mut().remove_child(len - 2);
        } else {
            self.last_status_pair = None;
        }
        let spacer_index = self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        let text_index = self.add_to_chat(Box::new(Text::new(
            theme::theme().fg(pi_tui::theme::ThemeColor::Muted, message),
            1,
            0,
        )));
        self.last_status_pair = Some((
            self.chat_row_addr(spacer_index).unwrap_or_default(),
            self.chat_row_addr(text_index).unwrap_or_default(),
        ));
        self.tui.request_render();
    }

    /// `showError` (`interactive-mode.ts:4809-4813`): a red line in the chat
    /// — `Spacer(1)` + `Error: ` + the message at the output pad.
    fn show_error(&mut self, message: &str) {
        self.add_to_chat(Box::new(pi_tui::components::spacer::Spacer::new(1)));
        self.add_to_chat(Box::new(Text::new(
            theme::theme().fg(
                pi_tui::theme::ThemeColor::Error,
                &format!("Error: {message}"),
            ),
            self.output_pad,
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
            // The resize tick (`process.stdout.on("resize", …)`): pi-tui
            // has no signal watcher, so the loop polls the real size every
            // iteration; the tick is drift-guarded and renders nothing
            // when nothing changed.
            let _ = self.tui.apply_resize();
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
    /// with their markers for the editor (`terminal.ts:252-278`). With
    /// extension terminal-input listeners registered (E3 slice 7) the chunk
    /// folds through the engine first — a busy engine defers it to a later
    /// frame rather than blocking the input thread.
    fn dispatch_stdin_events(&mut self, events: Vec<StdinEvent>) {
        for event in events {
            match event {
                StdinEvent::Data(sequence) => {
                    #[cfg(feature = "ext-js")]
                    if self.terminal_input_listeners > 0 {
                        self.dispatch_extension_terminal_input(sequence);
                        continue;
                    }
                    self.tui.handle_input(&sequence);
                }
                StdinEvent::Paste(content) => {
                    let wrapped = format!("\x1b[200~{content}\x1b[201~");
                    // The listeners see the wrapped chunk — the same bytes
                    // the editor receives (the wrap is the port's
                    // re-assembly of the decoded paste).
                    #[cfg(feature = "ext-js")]
                    if self.terminal_input_listeners > 0 {
                        self.dispatch_extension_terminal_input(wrapped);
                        continue;
                    }
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
        let displaced = self
            .replace_editor_slot(component)
            .expect("the editor slot holds a component");
        self.park_displaced_occupant(displaced);
        self.tui.set_focus(Some(FocusTarget::Id(focused)));
        self.tui.request_render();
    }

    // --------------------------------------------------------------
    // Small ctx.ui surfaces (E3 slice 7): widgets, footer/header,
    // the status family, editor text, tools expansion, terminal input
    // --------------------------------------------------------------

    /// Swap `component` into a dock or flat slot and return the previous
    /// occupant — [`Self::replace_editor_slot`] generalized (E3 slice 7's
    /// footer/header swaps ride it too).
    fn swap_slot_component(
        &mut self,
        slot: HostSlot,
        component: Box<dyn Component>,
    ) -> Option<Box<dyn Component>> {
        let dock_id = self.viewport.as_ref().map(ChatViewport::dock_id);
        if let Some(dock_id) = dock_id {
            return self
                .tui
                .component_by_id_deep_mut(dock_id)?
                .as_any_mut()?
                .downcast_mut::<VStack>()?
                .stack()
                .replace_child(slot.dock_index(), component)
                .into();
        }
        Some(self.tui.replace_child(slot.index(), component))
    }

    /// The built-in footer, mounted or displaced (`this.footer` stays
    /// reachable while a custom footer occupies the slot, `interactive-mode
    /// .ts:2682`): the refreshes keep landing so a restore shows current
    /// data.
    fn footer_mut(&mut self) -> Option<&mut FooterComponent> {
        // The probe-then-return shape: returning the first borrow directly
        // would pin it to the function's lifetime and collide with the
        // displaced-chrome borrow below.
        if self
            .downcast_tree::<FooterComponent>(HostSlot::Footer)
            .is_some()
        {
            return self.downcast_tree::<FooterComponent>(HostSlot::Footer);
        }
        #[cfg(feature = "ext-js")]
        if let Some(chrome) = self.custom_footer.as_mut() {
            return chrome
                .displaced
                .as_any_mut()?
                .downcast_mut::<FooterComponent>();
        }
        None
    }

    /// The built-in header, mounted or displaced — the same story as
    /// [`Self::footer_mut`].
    fn header_mut(&mut self) -> Option<&mut BuiltInHeader> {
        if self
            .downcast_tree::<BuiltInHeader>(HostSlot::Header)
            .is_some()
        {
            return self.downcast_tree::<BuiltInHeader>(HostSlot::Header);
        }
        #[cfg(feature = "ext-js")]
        if let Some(chrome) = self.custom_header.as_mut() {
            return chrome
                .displaced
                .as_any_mut()?
                .downcast_mut::<BuiltInHeader>();
        }
        None
    }

    /// `setExtensionFooter` (`interactive-mode.ts:2664-2686`): dispose the
    /// existing custom footer, then swap the slot — the factory's adopted
    /// component, or the built-in back (`undefined`). The built-in stays
    /// alive while displaced and keeps taking the refresh writes.
    #[cfg(feature = "ext-js")]
    fn set_extension_footer(&mut self, handle: Option<u32>) {
        if let Some(ExtensionChrome {
            handle: old,
            displaced,
        }) = self.custom_footer.take()
        {
            // The built-in returns to the slot, pulling the old proxy out;
            // the dispose lands with neither in the tree (upstream disposes
            // before the swap).
            drop(self.swap_slot_component(HostSlot::Footer, displaced));
            self.dispose_extension_handle(old);
        }
        if let Some(handle) = handle
            && let Some(runner) = self.extension_runner.as_ref().map(Arc::clone)
        {
            let proxy = Box::new(ComponentProxy::borrowed(runner, handle, true));
            if let Some(displaced) = self.swap_slot_component(HostSlot::Footer, proxy) {
                self.custom_footer = Some(ExtensionChrome { handle, displaced });
            }
        }
        self.tui.request_render();
    }

    /// `setExtensionHeader` (`interactive-mode.ts:2691-2730`): the same
    /// swap for the header, with the duck-typed `setExpanded` the mounted
    /// custom header gets (`isExpandable`, `:2709-2711`).
    #[cfg(feature = "ext-js")]
    fn set_extension_header(&mut self, handle: Option<u32>) {
        if let Some(ExtensionChrome {
            handle: old,
            displaced,
        }) = self.custom_header.take()
        {
            drop(self.swap_slot_component(HostSlot::Header, displaced));
            self.dispose_extension_handle(old);
        }
        if let Some(handle) = handle
            && let Some(runner) = self.extension_runner.as_ref().map(Arc::clone)
        {
            let proxy = ComponentProxy::borrowed(runner, handle, true);
            proxy.set_expanded(self.tool_output_expanded);
            if let Some(displaced) = self.swap_slot_component(HostSlot::Header, Box::new(proxy)) {
                self.custom_header = Some(ExtensionChrome { handle, displaced });
            }
        }
        self.tui.request_render();
    }

    /// One adopted component's dispose through the bridge (the
    /// `dispose?.()` + registry drop, `interactive-mode.ts:2670-2672`).
    #[cfg(feature = "ext-js")]
    fn dispose_extension_handle(&mut self, handle: u32) {
        if let Some(runner) = self.extension_runner.as_ref() {
            let _ = runner
                .lock()
                .expect("extension runner mutex poisoned")
                .component_dispose(handle);
        }
    }

    /// `setExtensionWidget` (`interactive-mode.ts:2538-2578`): the key
    /// removes from BOTH placements first (a displaced adopted component
    /// disposes), `None` content removes, and the render follows.
    #[cfg(feature = "ext-js")]
    fn set_extension_widget(
        &mut self,
        key: &str,
        content: Option<ExtensionWidget>,
        placement: &str,
    ) {
        // The key removes from BOTH placements (`removeExisting`); the
        // removals collect first so the disposes run without a live
        // borrow of either map.
        let mut displaced: Vec<ExtensionWidget> = Vec::new();
        if let Some(index) = self
            .extension_widgets_above
            .iter()
            .position(|(existing, _)| existing == key)
        {
            displaced.push(self.extension_widgets_above.remove(index).1);
        }
        if let Some(index) = self
            .extension_widgets_below
            .iter()
            .position(|(existing, _)| existing == key)
        {
            displaced.push(self.extension_widgets_below.remove(index).1);
        }
        for widget in &displaced {
            self.dispose_extension_widget(widget);
        }
        if let Some(content) = content {
            let target = if placement == "belowEditor" {
                &mut self.extension_widgets_below
            } else {
                &mut self.extension_widgets_above
            };
            target.push((key.to_string(), content));
        }
        self.render_extension_widgets();
    }

    /// One widget's teardown (`removeExisting`'s `dispose?.()`,
    /// `interactive-mode.ts:2544-2548`): an adopted handle disposes through
    /// the bridge.
    #[cfg(feature = "ext-js")]
    fn dispose_extension_widget(&mut self, widget: &ExtensionWidget) {
        if let ExtensionWidget::Adopted { handle, .. } = widget {
            self.dispose_extension_handle(*handle);
        }
    }

    /// `renderWidgets`/`renderWidgetContainer` (`interactive-mode.ts
    /// :2631-2659`): rebuild both hosts — the above host renders a spacer
    /// when empty and a leading one when filled; the below host never
    /// spacers. Upstream re-adds the same component objects; the port
    /// rebuilds (Text rows from the stored lines, borrowed views over the
    /// adopted handle).
    #[cfg(feature = "ext-js")]
    fn render_extension_widgets(&mut self) {
        let above = self.extension_widgets_above.clone();
        let below = self.extension_widgets_below.clone();
        let runner = self.extension_runner.clone();
        let fill = |container: &mut pi_tui::component::Container,
                    entries: &[(String, ExtensionWidget)],
                    spacer_when_empty: bool,
                    leading_spacer: bool| {
            container.clear();
            if entries.is_empty() {
                if spacer_when_empty {
                    container.add_child(Box::new(pi_tui::components::spacer::Spacer::new(1)));
                }
                return;
            }
            if leading_spacer {
                container.add_child(Box::new(pi_tui::components::spacer::Spacer::new(1)));
            }
            for (_, widget) in entries {
                match widget {
                    ExtensionWidget::Lines(lines) => {
                        for line in lines.iter().take(MAX_WIDGET_LINES) {
                            container.add_child(Box::new(Text::new(line.clone(), 1, 0)));
                        }
                        if lines.len() > MAX_WIDGET_LINES {
                            container.add_child(Box::new(Text::new(
                                theme::theme()
                                    .fg(pi_tui::theme::ThemeColor::Muted, "... (widget truncated)"),
                                1,
                                0,
                            )));
                        }
                    }
                    ExtensionWidget::Adopted { handle, has_input } => {
                        if let Some(runner) = runner.as_ref() {
                            container.add_child(Box::new(ComponentProxy::borrowed(
                                Arc::clone(runner),
                                *handle,
                                *has_input,
                            )));
                        }
                    }
                }
            }
        };
        if let Some(container) =
            self.downcast_tree::<pi_tui::component::Container>(HostSlot::WidgetAbove)
        {
            fill(container, &above, true, true);
        }
        if let Some(container) =
            self.downcast_tree::<pi_tui::component::Container>(HostSlot::WidgetBelow)
        {
            fill(container, &below, false, false);
        }
        self.tui.request_render();
    }

    /// `clearExtensionWidgets` (`interactive-mode.ts:2580-2590`).
    #[cfg(feature = "ext-js")]
    fn clear_extension_widgets(&mut self) {
        let above = std::mem::take(&mut self.extension_widgets_above);
        let below = std::mem::take(&mut self.extension_widgets_below);
        for (_, widget) in above.into_iter().chain(below) {
            self.dispose_extension_widget(&widget);
        }
        self.render_extension_widgets();
    }

    /// `setWorkingMessage` (`interactive-mode.ts:2781-2786`): the field and
    /// the live working indicator's message (`undefined` = the default).
    fn set_working_message(&mut self, message: Option<String>) {
        self.working_message = message;
        let mut status = self.status_handle.lock().expect("status");
        if let Some(indicator) = status.as_mut()
            && indicator.kind == StatusKind::Working
        {
            indicator.set_message(
                self.working_message
                    .clone()
                    .unwrap_or_else(|| "Working".to_string()),
            );
        }
        drop(status);
        self.tui.request_render();
    }

    /// `setWorkingVisible` (`interactive-mode.ts:2501-2512`): false clears
    /// a working indicator, true re-shows one while a run streams.
    fn set_working_visible(&mut self, visible: bool) {
        self.working_visible = visible;
        if !visible {
            let mut status = self.status_handle.lock().expect("status");
            if status
                .as_ref()
                .is_some_and(|indicator| indicator.kind == StatusKind::Working)
            {
                *status = None;
            }
            drop(status);
        } else if self.agent_run.is_some() {
            let active_working = self
                .status_handle
                .lock()
                .expect("status")
                .as_ref()
                .is_some_and(|indicator| indicator.kind == StatusKind::Working);
            if !active_working {
                let indicator = self.build_working_status_indicator();
                *self.status_handle.lock().expect("status") = Some(indicator);
            }
        }
        self.tui.request_render();
    }

    /// `showWorkingStatusIndicator` (`interactive-mode.ts:2486-2499`): the
    /// working message (default `Working`) and the indicator options — the
    /// embedded-border coloring rides the port's decorator.
    fn build_working_status_indicator(&self) -> StatusIndicator {
        StatusIndicator::working_with(
            self.working_message
                .clone()
                .unwrap_or_else(|| "Working".to_string()),
            self.working_indicator_options.clone(),
        )
    }

    /// `setWorkingIndicator` (`interactive-mode.ts:2514-2520`): the field
    /// and the live working indicator's frames.
    fn set_working_indicator(
        &mut self,
        options: Option<pi_tui::components::loader::LoaderIndicator>,
    ) {
        self.working_indicator_options.clone_from(&options);
        let mut status = self.status_handle.lock().expect("status");
        if let Some(indicator) = status.as_mut()
            && indicator.kind == StatusKind::Working
        {
            indicator.set_indicator(options);
        }
        drop(status);
        self.tui.request_render();
    }

    /// `setHiddenThinkingLabel` (`interactive-mode.ts:2522-2533`): the
    /// field and every assistant row's label (`undefined` = the default);
    /// the streaming row is an assistant row here, so the chat walk covers
    /// it.
    fn set_hidden_thinking_label(&mut self, label: Option<String>) {
        self.hidden_thinking_label = label.unwrap_or_else(|| "Thinking...".to_string());
        let label = self.hidden_thinking_label.clone();
        let len = self.chat_len();
        for index in 0..len {
            if let Some(row) = self.downcast_chat::<AssistantMessageComponent>(index) {
                row.set_hidden_thinking_label(&label);
            }
        }
        self.tui.request_render();
    }

    /// `pasteToEditor` (`interactive-mode.ts:2795`): the text rides the
    /// bracketed-paste markers through the editor's handleInput.
    #[cfg(feature = "ext-js")]
    fn paste_to_editor(&mut self, text: &str) {
        let wrapped = format!("\x1b[200~{text}\x1b[201~");
        if let Some(editor) = self.editor_host_mut() {
            editor.handle_input(&wrapped);
        }
        self.tui.request_render();
    }

    /// `setEditorText` (`interactive-mode.ts:2796`): the active editor's
    /// `setText` — the delegate's when a component is live (E3 slice 9).
    #[cfg(feature = "ext-js")]
    fn set_editor_text(&mut self, text: &str) {
        if let Some(live) = self.extension_editor.lock().expect("extension editor").as_mut() {
            live.set_text(text);
        } else if let Some(editor) = self.editor_host_mut() {
            editor.editor.set_text(text);
        }
        self.tui.request_render();
    }

    // ---- E3 slice 9: the extension editor component ----

    /// Mounts the extension editor component (the `setEditorComponent`
    /// arm): the delegate adopts the handle with the snapshot text, and
    /// the working status moves to the status host row (`isEmbeddedStatus`
    /// fails for a custom component upstream — `interactive-mode.ts
    /// :2445-2455`).
    #[cfg(feature = "ext-js")]
    fn mount_extension_editor_component(&mut self, handle: u32) {
        let Some(runner) = self.extension_runner.as_ref().map(Arc::clone) else {
            return;
        };
        // The swap copies the previous editor's text (`interactive-mode.ts
        // :2984/:2999-3000`): the snapshot mirror, refreshed first so the
        // read is current for this frame (≤ 1 frame stale across frames —
        // documented).
        self.refresh_ui_snapshot();
        let text = self
            .ui_snapshot
            .lock()
            .expect("ui snapshot")
            .editor_text
            .clone();
        let mut live = self.extension_editor.lock().expect("extension editor");
        if let Some(previous) = live.as_ref() {
            previous.release();
        }
        *live = Some(ExtensionEditorLive::new(runner, handle, text));
        drop(live);
        self.sync_extension_editor_status_row();
        self.tui.request_render();
    }

    /// Restores the built-in editor (`setCustomEditorComponent(undefined)`,
    /// `interactive-mode.ts:3049-3053`): the custom editor's text moves
    /// back, the delegate drops, and the status embedding reverts.
    #[cfg(feature = "ext-js")]
    fn restore_default_editor_component(&mut self) {
        let text = self
            .extension_editor
            .lock()
            .expect("extension editor")
            .as_mut()
            .map(|live| live.editor_text())
            .unwrap_or_default();
        if let Some(live) = self.extension_editor.lock().expect("extension editor").take() {
            live.release();
        }
        if let Some(editor) = self.editor_host_mut() {
            editor.editor.set_text(&text);
        }
        self.sync_extension_editor_status_row();
        self.tui.request_render();
    }

    /// The working-status row: mounted while a delegate is live (upstream's
    /// non-embedded branch puts the indicator in `statusContainer`),
    /// removed when the built-in editor returns. The decorator path is
    /// unchanged — the row only renders the same shared indicator.
    #[cfg(feature = "ext-js")]
    fn sync_extension_editor_status_row(&mut self) {
        let delegate_live = self
            .extension_editor
            .lock()
            .expect("extension editor")
            .is_some();
        if delegate_live == self.extension_editor_status_row {
            return;
        }
        let Some(host) = self.downcast_tree::<pi_tui::component::Container>(HostSlot::Status)
        else {
            return;
        };
        host.clear();
        if delegate_live {
            host.add_child(Box::new(ExtensionEditorStatusRow {
                status: Arc::clone(&self.status_handle),
            }));
        }
        self.extension_editor_status_row = delegate_live;
    }

    /// The composed-provider answer for the wire's autocomplete adapter:
    /// the same shape the built-in editor's provider serves
    /// (`AutocompleteSuggestions` — items + prefix).
    #[cfg(feature = "ext-js")]
    fn extension_editor_autocomplete(&mut self, request: &serde_json::Value) -> serde_json::Value {
        let lines: Vec<String> = request
            .get("lines")
            .and_then(Value::as_array)
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let cursor_line = request.get("cursorLine").and_then(Value::as_u64).unwrap_or(0) as usize;
        let cursor_col = request.get("cursorCol").and_then(Value::as_u64).unwrap_or(0) as usize;
        let force = request.get("force").and_then(Value::as_bool).unwrap_or(false);
        // The composed chain: the base wrapped through the registered JS
        // wrappers (E3 slice 8) — the same provider the built-in editor
        // uses, exactly what upstream passes (`this.autocompleteProvider`).
        let provider = self.composed_autocomplete_provider();
        let suggestions =
            provider.and_then(|provider| provider.suggest(&lines, cursor_line, cursor_col, force));
        let Some(suggestions) = suggestions else {
            return serde_json::json!({});
        };
        serde_json::json!({
            "result": {
                "items": suggestions
                    .items
                    .iter()
                    .map(|item| {
                        serde_json::json!({
                            "value": item.value,
                            "label": item.label,
                            "description": item.description,
                        })
                    })
                    .collect::<Vec<_>>(),
                "prefix": suggestions.prefix,
            }
        })
    }

    /// The duck-typed `AppAction` resolution for the `editorAction`
    /// crossings (`app:*` ids — the port's app_keys table).
    #[cfg(feature = "ext-js")]
    fn app_action_from_id(id: &str) -> Option<AppAction> {
        match id {
            theme::app_keys::CLEAR => Some(AppAction::Clear),
            theme::app_keys::EXIT => Some(AppAction::Exit),
            theme::app_keys::INTERRUPT => Some(AppAction::Interrupt),
            theme::app_keys::THINKING_CYCLE => Some(AppAction::CycleThinking),
            theme::app_keys::MODEL_CYCLE_FORWARD => Some(AppAction::CycleModel(1)),
            theme::app_keys::MODEL_CYCLE_BACKWARD => Some(AppAction::CycleModel(-1)),
            theme::app_keys::THINKING_TOGGLE => Some(AppAction::ToggleThinking),
            theme::app_keys::TOOLS_EXPAND => Some(AppAction::ToolsExpand),
            theme::app_keys::MESSAGE_FOLLOW_UP => Some(AppAction::FollowUp),
            theme::app_keys::MESSAGE_DEQUEUE => Some(AppAction::Dequeue),
            _ => None,
        }
    }

    /// `WorkingIndicatorOptions` (`types.ts:136-141`) off the crossing.
    #[cfg(feature = "ext-js")]
    fn loader_indicator_from_json(
        value: &serde_json::Value,
    ) -> Option<pi_tui::components::loader::LoaderIndicator> {
        let frames = value
            .get("frames")
            .and_then(serde_json::Value::as_array)
            .map(|frames| {
                frames
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(String::from)
                    .collect::<Vec<_>>()
            });
        let interval_ms = value.get("intervalMs").and_then(serde_json::Value::as_u64);
        if frames.is_none() && interval_ms.is_none() {
            return None;
        }
        Some(pi_tui::components::loader::LoaderIndicator {
            frames: frames.unwrap_or_default(),
            interval_ms: interval_ms.unwrap_or(0),
        })
    }

    /// The terminal-input listener fold (E3 slice 7): the chunk folds
    /// through the engine's registered handlers (`tui.ts:1095-1108`); a
    /// busy engine (settling a dialog holds the runner mutex) defers the
    /// chunk — the pump retries each frame, upstream-equivalent to the
    /// single-threaded engine finishing its handler first.
    #[cfg(feature = "ext-js")]
    fn dispatch_extension_terminal_input(&mut self, data: String) {
        match self.try_terminal_input_fold(&data) {
            TerminalInputFold::Deliver(transformed) => {
                if !transformed.is_empty() {
                    self.tui.handle_input(&transformed);
                }
            }
            TerminalInputFold::Consumed => {}
            TerminalInputFold::Busy => self.deferred_terminal_input.push(data),
        }
    }

    /// One fold attempt: the engine was busy (defer), a listener consumed
    /// the chunk, or this is what the dispatch sees.
    #[cfg(feature = "ext-js")]
    fn try_terminal_input_fold(&mut self, data: &str) -> TerminalInputFold {
        let Some(runner) = self.extension_runner.as_ref() else {
            return TerminalInputFold::Busy;
        };
        let Some(runner) = runner.try_lock().ok() else {
            return TerminalInputFold::Busy;
        };
        // A fold error behaves like a consumed chunk — one broken
        // extension cannot wedge the input path.
        match runner.deliver_terminal_input(data) {
            Ok(Some(transformed)) => TerminalInputFold::Deliver(transformed),
            Ok(None) | Err(_) => TerminalInputFold::Consumed,
        }
    }

    /// The deferred chunks retry each frame (`pump`).
    #[cfg(feature = "ext-js")]
    fn pump_deferred_terminal_input(&mut self) {
        if self.deferred_terminal_input.is_empty() {
            return;
        }
        let deferred = std::mem::take(&mut self.deferred_terminal_input);
        let mut remaining = Vec::new();
        for data in deferred {
            match self.try_terminal_input_fold(&data) {
                TerminalInputFold::Deliver(transformed) => {
                    if !transformed.is_empty() {
                        self.tui.handle_input(&transformed);
                    }
                }
                TerminalInputFold::Consumed => {}
                TerminalInputFold::Busy => remaining.push(data),
            }
        }
        self.deferred_terminal_input = remaining;
    }

    /// The snapshot refresh (E3 slice 7): the footer cells, the editor's
    /// expanded text, and the tools flag the synchronous ops read — one
    /// pump frame stale, the mode's own render cadence. E3 slice 9 adds
    /// the editor chrome (padding, autocomplete max visible, app action
    /// ids, border color token) and the delegate's text mirror.
    #[cfg(feature = "ext-js")]
    fn refresh_ui_snapshot(&mut self) {
        let footer_json = self.footer_mut().map(|footer| {
            serde_json::json!({
                "gitBranch": footer.data.git_branch,
                "extensionStatuses": footer.data.extension_statuses,
                "availableProviderCount": footer.data.available_provider_count,
            })
        });
        // The delegate's text mirror refreshes here (a busy engine keeps
        // the previous mirror); the built-in editor reads directly.
        let delegate_text = self
            .extension_editor
            .lock()
            .expect("extension editor")
            .as_mut()
            .map(|live| live.editor_text());
        let editor_text = delegate_text.unwrap_or_else(|| {
            self.editor_host_mut()
                .map_or(String::new(), |editor| editor.editor.get_expanded_text())
        });
        let (padding_x, max_visible) = self
            .editor_host_mut()
            .map_or((0, 5), |editor| {
                (
                    editor.editor.padding_x,
                    editor.editor.autocomplete_max_visible,
                )
            });
        let border_color = if self
            .editor_host_mut()
            .is_some_and(|editor| editor.is_bash_mode)
        {
            "bashMode".to_string()
        } else {
            format!("thinking{}", Self::thinking_color_suffix(self.thinking_level))
        };
        let app_actions: Vec<String> = theme::app_keys::DEFAULTS
            .iter()
            .map(|(id, _)| (*id).to_string())
            .collect();
        let mut snapshot = self.ui_snapshot.lock().expect("ui snapshot");
        if let Some(footer) = footer_json {
            snapshot.footer = footer;
        }
        snapshot.editor_text = editor_text;
        snapshot.tools_expanded = self.tool_output_expanded;
        snapshot.editor_padding_x = padding_x;
        snapshot.editor_max_visible = max_visible;
        snapshot.editor_border_color = border_color;
        snapshot.app_actions = app_actions;
    }

    /// The camelCase `ThemeColor` name for the thinking border token
    /// (`thinking_border_styler`, `theme.rs:709-721` — the tokens are
    /// `thinkingOff`/`thinkingMinimal`/… camelCase).
    #[cfg(feature = "ext-js")]
    fn thinking_color_suffix(level: ThinkingLevel) -> &'static str {
        match level {
            ThinkingLevel::Off => "Off",
            ThinkingLevel::Minimal => "Minimal",
            ThinkingLevel::Low => "Low",
            ThinkingLevel::Medium => "Medium",
            ThinkingLevel::High => "High",
            ThinkingLevel::Xhigh => "Xhigh",
            ThinkingLevel::Max => "Max",
        }
    }

    /// `setupAutocompleteProvider` (`interactive-mode.ts:853-867`): the
    /// base provider wraps L→R through the registered wrappers; a
    /// non-empty trigger union replaces the provider's own characters
    /// (`[...new Set(triggerCharacters)]`), first occurrence first. The
    /// composed provider re-mounts on the editor — mounted or parked,
    /// like upstream's `defaultEditor` + non-default `this.editor`.
    #[cfg(feature = "ext-js")]
    fn rebuild_autocomplete_provider(&mut self) {
        let mut current: Arc<dyn pi_tui::components::editor::AutocompleteProvider> =
            Arc::clone(&self.base_autocomplete_provider);
        let mut union: Vec<char> = Vec::new();
        for record in &self.autocomplete_wrappers {
            let Some(runner) = self.extension_runner.as_ref().map(Arc::clone) else {
                return;
            };
            union.extend(record.trigger_characters.iter().copied());
            current = Arc::new(JsAutocompleteProvider {
                runner,
                index: record.index,
                base: current,
            });
        }
        let composed: Arc<dyn pi_tui::components::editor::AutocompleteProvider> =
            if union.is_empty() {
                current
            } else {
                let mut unique: Vec<char> = Vec::new();
                for character in union {
                    if !unique.contains(&character) {
                        unique.push(character);
                    }
                }
                Arc::new(ComposedAutocompleteProvider {
                    provider: current,
                    trigger_characters: unique,
                })
            };
        if let Some(editor) = self.editor_host_mut() {
            editor
                .editor
                .set_autocomplete_provider(Some(Arc::clone(&composed)));
        }
        // The non-default editor re-sets too (`setupAutocompleteProvider`
        // re-calls `setAutocompleteProvider` on `this.editor` when it is
        // not the default — `interactive-mode.ts:855-871`): the delegate's
        // provider is a fresh composed adapter each rebuild.
        if let Some(live) = self.extension_editor.lock().expect("extension editor").as_ref() {
            live.set_autocomplete_provider();
        }
        self.tui.request_render();
    }

    /// The composed chain the autocomplete reads serve (E3 slice 8 + 9):
    /// the base wrapped L→R through the registered wrappers, the trigger
    /// union replacing the provider's own characters when non-empty.
    /// `None` when the runner is gone (a wrapper could not dispatch).
    #[cfg(feature = "ext-js")]
    fn composed_autocomplete_provider(
        &self,
    ) -> Option<Arc<dyn pi_tui::components::editor::AutocompleteProvider>> {
        let mut current: Arc<dyn pi_tui::components::editor::AutocompleteProvider> =
            Arc::clone(&self.base_autocomplete_provider);
        let mut union: Vec<char> = Vec::new();
        for record in &self.autocomplete_wrappers {
            let runner = Arc::clone(self.extension_runner.as_ref()?);
            union.extend(record.trigger_characters.iter().copied());
            current = Arc::new(JsAutocompleteProvider {
                runner,
                index: record.index,
                base: current,
            });
        }
        if union.is_empty() {
            return Some(current);
        }
        let mut unique: Vec<char> = Vec::new();
        for character in union {
            if !unique.contains(&character) {
                unique.push(character);
            }
        }
        Some(Arc::new(ComposedAutocompleteProvider {
            provider: current,
            trigger_characters: unique,
        }))
    }

    /// Where the displaced editor-slot occupant goes (the slot-swap
    /// invariant: `suspended_editor` always holds the `ChatEditor`, so a
    /// built-in selector opened over an open extension dialog never
    /// destroys the parked editor):
    /// - the `ChatEditor`: back into `suspended_editor`;
    /// - the visible top extension dialog: into its entry — upstream
    ///   keeps the dialog in `extensionDialogs` while a selector replaces
    ///   it visually and leaves its promise pending
    ///   (`interactive-mode.ts:5088`, no dialogOpen check there);
    /// - a built-in selector displaced by another: dropped, as before.
    fn park_displaced_occupant(&mut self, mut displaced: Box<dyn Component>) {
        if displaced
            .as_any_mut()
            .is_some_and(|any| any.is::<ChatEditor>())
        {
            self.suspended_editor = Some(displaced);
            return;
        }
        #[cfg(feature = "ext-js")]
        if !self.extension_ui_dialogs.is_empty()
            && self
                .extension_ui_dialogs
                .last()
                .is_some_and(|top| top.component.is_none())
        {
            let top = self
                .extension_ui_dialogs
                .last_mut()
                .expect("stack top just checked");
            top.component = Some(displaced);
            return;
        }
        drop(displaced);
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
        self.user_message_selector_events = None;
        if let Some(navigation) = self.tree_navigation.take()
            && let Some(run) = navigation.summary_run
        {
            run.abort.abort();
        }
        self.tree_selector_events = None;
        self.tree_summary_dialog_events = None;
        self.tree_prompt_editor_events = None;
        self.import_selector_events = None;
        self.import_dialog = None;
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
                // No extension-UI reset: upstream runs `resetExtensionUI`
                // at init (`interactive-mode.ts:643`) and on `/reload`
                // (`:6628`) only — a session switch leaves pending dialogs
                // and their promises exactly as they are, because the
                // extensions themselves persist across the switch (they
                // get `session_shutdown`/`session_start` instead). The
                // port matches: mounted dialogs stay interactive over the
                // resumed transcript, and their delivery threads hold no
                // runner mutex while waiting (`begin_ui_dialog`), so the
                // switch's own emits cannot deadlock on them.
                // The rebind callback re-renders the CURRENT session state:
                // `renderCurrentSessionState` clears the chat before replaying
                // the resumed transcript (`interactive-mode.ts:2337-2347` via
                // `rebindCurrentSession({renderBeforeBind: true})`), so the
                // resumed entries REPLACE the old transcript rather than
                // appending below it.
                self.chat_mut().clear();
                self.last_status_pair = None;
                self.render_initial_messages();
                self.refresh_footer();
                // `rebindCurrentSession`'s program-status reset
                // (`interactive-mode.ts:2222`): the resumed session reports
                // idle until its own run settles.
                let session_name = self.session.get_session_name();
                self.program_status
                    .reset(session_name.as_deref(), &mut self.tui);
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
    // /export and /import (`handleExportCommand` `interactive-mode.ts:6708-6724`,
    // `getPathCommandArgument` `:6726-6750`, `handleImportCommand`
    // `:6752-6803`, `AgentSessionRuntime.importFromJsonl`
    // `agent-session-runtime.ts:359-407`, and `switchToForkedSession`
    // `interactive-mode.ts:5982-5999`)
    // --------------------------------------------------------------

    /// `getPathCommandArgument` (`interactive-mode.ts:6726-6750`): the first
    /// argument of `/export`/`/import` — a quoted span or a whitespace
    /// delimited token.
    fn get_path_command_argument<'a>(text: &'a str, command: &str) -> Option<&'a str> {
        if text == command {
            return None;
        }
        // Only `command` followed by a space parses; `/importer …` does not.
        let args_string = text.strip_prefix(command)?.strip_prefix(' ')?;
        let args_string = args_string.trim_start();
        if args_string.is_empty() {
            return None;
        }

        let first_char = args_string.chars().next()?;
        if first_char == '"' || first_char == '\'' {
            let closing_quote = args_string[first_char.len_utf8()..].find(first_char)?;
            return Some(
                &args_string[first_char.len_utf8()..first_char.len_utf8() + closing_quote],
            );
        }

        // `search(/\s/)` < 0 returns the whole argument
        // (`interactive-mode.ts:6745-6747`) — apostrophes and other
        // non-whitespace bytes survive unquoted.
        let Some(first_whitespace) = args_string.find(char::is_whitespace) else {
            return Some(args_string);
        };
        Some(&args_string[..first_whitespace])
    }

    /// `/reload` (`handleReloadCommand`, `interactive-mode.ts:6618-6706`)
    /// over the session reload sequence (`session.reload`,
    /// `agent-session.ts:3828-3874`). Mechanism deviations (HANDOFF.md):
    /// the port's reload is synchronous on the UI thread, so upstream's
    /// bordered "Reloading…" editor box — which covers the async gap — has
    /// no observable frame here; the default-tools delta rides a tool
    /// registry the port has not ported; the implicit project-trust save
    /// after a reload is startup-only in the port, so the status never
    /// carries the "saved project trust" suffix; keybindings are static
    /// constants (`theme::app_keys`), so `keybindings.reload()` is a no-op.
    #[allow(clippy::too_many_lines)] // mirrors the TS handleReloadCommand; split when extending
    fn handle_reload_command(&mut self) {
        if self.agent_run.is_some() {
            self.show_warning("Wait for the current response to finish before reloading.");
            return;
        }
        if self.is_compacting() {
            self.show_warning("Wait for compaction to finish before reloading.");
            return;
        }

        // `resetExtensionUI` (`interactive-mode.ts:6628`): a pending
        // extension login flow cancels and every open dialog disposes.
        // Today the dialogs arm is unreachable through the UI — the
        // editor owns input while `/reload` is typed, and a mounted
        // dialog parks the editor — but the port runs it anyway, exactly
        // like upstream, so the invariant never depends on that reach.
        #[cfg(feature = "ext-js")]
        self.cancel_extension_login_flow();
        #[cfg(feature = "ext-js")]
        self.reset_extension_ui_dialogs();
        // `rebindCurrentSession`'s program-status reset
        // (`interactive-mode.ts:2222`): forget the previous session's run.
        {
            let session_name = self.session.get_session_name();
            self.program_status
                .reset(session_name.as_deref(), &mut self.tui);
        }

        // `session.reload` (`agent-session.ts:3838-3849`): the shutdown
        // event, the invalidation, the settings reload, the queue-mode
        // sync.
        self.emit_extension_shutdown("reload", None);
        #[cfg(feature = "ext-js")]
        if let Some(runner) = &self.extension_runner
            && let Err(error) = runner
                .lock()
                .expect("extension runner mutex poisoned")
                .invalidate(None)
        {
            eprintln!("Warning: Extension error: invalidate: {error}");
        }
        self.settings.reload();
        // `syncQueueModesFromSettings` (`agent-session.ts:2863+`): both
        // queue modes re-read from the reloaded settings.
        {
            let steering = self
                .settings
                .get("steeringMode")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let follow_up = self
                .settings
                .get("followUpMode")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let mut queues = self.queues.lock().expect("queues");
            queues.set_steering_mode(queue_mode_from_setting(steering.as_deref()));
            queues.set_follow_up_mode(queue_mode_from_setting(follow_up.as_deref()));
        }

        // The extension half: the re-discovery against the reloaded
        // settings, the re-bind, `session_start(reason:"reload")`, and the
        // MCP reconnect (`resourceLoader.reload()` →
        // `agent-session.ts:3869-3872`); the diagnostics re-report like
        // startup's (`showLoadedResources`, `interactive-mode.ts:6684-6687`).
        #[cfg(feature = "ext-js")]
        let reload_diagnostics = match &self.extension_runner {
            Some(runner) => crate::extensions::reload(
                &self.extension_cli_paths,
                self.no_extensions,
                self.session.get_cwd(),
                &self.agent_dir,
                self.project_trusted,
                &self.settings,
                runner,
            ),
            None => Vec::new(),
        };
        #[cfg(not(feature = "ext-js"))]
        let reload_diagnostics: Vec<crate::cli::args::Diagnostic> = Vec::new();
        for diagnostic in &reload_diagnostics {
            if diagnostic.kind == DiagnosticKind::Warning {
                eprintln!("Warning: {}", diagnostic.message);
            }
        }
        for diagnostic in &reload_diagnostics {
            if diagnostic.kind == DiagnosticKind::Error {
                eprintln!("Error: {}", diagnostic.message);
            }
        }

        // Extension shortcuts re-resolve over the re-bound runner
        // (`setupExtensionShortcuts`, `interactive-mode.ts:6683`).
        #[cfg(feature = "ext-js")]
        self.setup_extension_shortcuts();
        // The markdown transformers re-resolve with the reloaded runner
        // (`getMarkdownTransformers` upstream).
        #[cfg(feature = "ext-js")]
        self.setup_markdown_transformers();

        // `resetApiProviders` + `_buildRuntime` (`agent-session.ts:3850-3862`):
        // the composed catalog recomposes over the reloaded models.json and
        // registrations — after the extension half, like upstream's
        // `_buildRuntime` follows `resourceLoader.reload()`.
        #[cfg(feature = "ext-js")]
        let refresh_errors = refresh_model_catalogs(self.extension_runner.as_ref()).errors;
        #[cfg(not(feature = "ext-js"))]
        let refresh_errors = refresh_model_catalogs().errors;

        // The theme half of `resourceLoader.reload()` + `setRegisteredThemes`
        // + `applyRuntimeSettings` + `themeController.applyFromSettings`
        // (`interactive-mode.ts:6678-6680`): the theme re-resolves from the
        // reloaded settings — the `--theme` flag stays a startup-only
        // override.
        let theme_name = self
            .settings
            .get("theme")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        match theme_name.as_deref() {
            Some(name) => {
                if let Err(error) = theme::set_theme(name) {
                    eprintln!("Warning: Theme error: {error}");
                }
            }
            None => theme::init_theme(None),
        }
        self.update_editor_border_color();
        // `applyRuntimeSettings` (`interactive-mode.ts:6664-6665`, the
        // restore half): the thinking-hide setting re-reads.
        self.hide_thinking_block = self
            .settings
            .get("hideThinkingBlock")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        // `models.json error` (`interactive-mode.ts:6689-6692` over
        // `model-runtime.getError()`, `model-runtime.ts:512-520`): the
        // config error plus the composition errors; the port's availability
        // rides the request-time credential ladder, so there is no
        // availability arm.
        {
            let models_config =
                crate::models_config::ModelConfig::load(&crate::config::get_models_path());
            let mut error_parts: Vec<String> = Vec::new();
            if let Some(config_error) = models_config.get_error() {
                error_parts.push(config_error.to_string());
            }
            for (provider_id, error) in &refresh_errors {
                error_parts.push(format!("Provider \"{provider_id}\": {error}"));
            }
            if !error_parts.is_empty() {
                self.show_error(&format!("models.json error: {}", error_parts.join("\n\n")));
            }
        }

        // Status (`interactive-mode.ts:6693-6697`).
        self.show_status(
            "Reloaded keybindings, extensions, skills, prompts, themes, and context files",
        );
    }

    /// `handleExportCommand` (`interactive-mode.ts:6708-6724`): a `.jsonl`
    /// argument exports the branch as JSONL, anything else (including no
    /// argument) exports HTML. The theme name rides the live theme
    /// (`theme.name` at `:6716-6718`); `agent-session.exportToHtml`'s
    /// settings fallback (`agent-session.ts:4471-4473`) is unreachable here
    /// because the port's theme always resolves.
    fn handle_export_command(&mut self, text: &str) {
        let output_path = Self::get_path_command_argument(text, "/export");

        // `outputPath?.endsWith(".jsonl")` (`interactive-mode.ts:6711`) —
        // case-sensitive, like upstream.
        let jsonl_output = output_path
            .filter(|path| path.strip_suffix(".jsonl").is_some())
            .map(std::path::Path::new);
        let result = if let Some(path) = jsonl_output {
            crate::session::export::export_session_to_jsonl(&self.session, Some(path))
                .map_err(|error| error.to_string())
        } else {
            let state = crate::session::export_html::ExportState {
                system_prompt: Some(self.system_prompt.clone()),
                tools: Some(self.export_tools()),
            };
            // The Arc must outlive the options: bind it before borrowing the
            // theme's name.
            let current_theme = theme::theme();
            crate::session::export_html::export_session_to_html(
                &self.session,
                Some(&state),
                &crate::session::export_html::ExportOptions {
                    output_path,
                    theme_name: current_theme.name(),
                    // The E1 extension boundary cannot run TUI tool renderers
                    // (see `session::export_html::tool_renderer`); custom
                    // tools fall back to the template's structured rendering.
                    tool_renderer: None,
                },
            )
        };
        match result {
            Ok(file_path) => {
                self.show_status(&format!("Session exported to: {}", file_path.display()));
            }
            Err(message) => self.show_error(&format!("Failed to export session: {message}")),
        }
    }

    /// `state.tools.map(t => ({name, description, parameters}))`
    /// (`agent-session.ts:4486`): the composed executor's declarations.
    fn export_tools(&self) -> Vec<crate::session::export_html::ExportTool> {
        self.executor
            .tools()
            .into_iter()
            .map(|tool| crate::session::export_html::ExportTool {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
            })
            .collect()
    }

    /// `handleImportCommand` up to the confirm dialog
    /// (`interactive-mode.ts:6755-6766`): the usage line, then the
    /// Yes/No gate.
    fn handle_import_command(&mut self, text: &str) {
        let Some(input_path) = Self::get_path_command_argument(text, "/import").map(str::to_string)
        else {
            self.show_error("Usage: /import <path.jsonl>");
            return;
        };

        if self.suspended_editor.is_some() {
            return;
        }
        self.open_import_selector(
            &format!("Import session\nReplace current session with {input_path}?"),
            vec!["Yes".to_string(), "No".to_string()],
            ImportDialog::Confirm { input_path },
        );
    }

    /// Mount the generic selector for the pending `/import` decision.
    fn open_import_selector(&mut self, title: &str, options: Vec<String>, dialog: ImportDialog) {
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = ExtensionSelectorComponent::new(title, options, Arc::clone(&events));
        self.import_selector_events = Some(events);
        self.import_dialog = Some(dialog);
        self.suspend_editor_with(Box::new(selector));
    }

    /// Put the editor back after an import decision or a cancel.
    fn close_import_selector(&mut self) {
        self.import_selector_events = None;
        self.import_dialog = None;
        self.restore_editor();
    }

    /// The mounted `/import` dialogs' events, drained like the other
    /// selectors. `handleImportCommand`'s continuation
    /// (`interactive-mode.ts:6767-6803`).
    fn pump_import_selector(&mut self) {
        if self.import_selector_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .import_selector_events
                .as_ref()
                .expect("import selector")
                .lock()
                .expect("import selector events"),
        );
        for event in events {
            let Some(dialog) = self.import_dialog.clone() else {
                continue;
            };
            match (dialog, event) {
                (
                    ImportDialog::Confirm { input_path },
                    ExtensionSelectorEvent::Selected(option),
                ) => {
                    self.close_import_selector();
                    if option != "Yes" {
                        self.show_status("Import cancelled");
                        continue;
                    }
                    // `clearStatusIndicator()` before the import
                    // (`interactive-mode.ts:6769`).
                    self.set_working_status(None);
                    match self.import_from_jsonl(&input_path, None) {
                        Ok(true) => self.show_status("Import cancelled"),
                        Ok(false) => {
                            self.show_status(&format!("Session imported from: {input_path}"));
                        }
                        Err(ImportFromJsonlError::MissingSessionCwd(issue)) => {
                            // `promptForMissingSessionCwd`
                            // (`interactive-mode.ts:6777-6792`).
                            let options =
                                crate::session::cwd::get_missing_session_cwd_options(&issue);
                            self.open_import_selector(
                                &crate::session::cwd::format_missing_session_cwd_prompt(&issue),
                                options.iter().map(|o| o.label.clone()).collect(),
                                ImportDialog::MissingSessionCwd { input_path, issue },
                            );
                        }
                        Err(error) => self.report_import_failure(error),
                    }
                }
                (ImportDialog::Confirm { .. }, ExtensionSelectorEvent::Cancel) => {
                    self.close_import_selector();
                    // A dismissed confirm resolves false.
                    self.show_status("Import cancelled");
                }
                (
                    ImportDialog::MissingSessionCwd { input_path, issue },
                    ExtensionSelectorEvent::Selected(option),
                ) => {
                    self.close_import_selector();
                    let choice = crate::session::cwd::get_missing_session_cwd_options(&issue)
                        .into_iter()
                        .find(|o| o.label == option)
                        .and_then(|o| o.value);
                    match choice {
                        Some(crate::session::cwd::MissingSessionCwdChoice::Continue) => {
                            let result =
                                self.import_from_jsonl(&input_path, Some(&issue.fallback_cwd));
                            match result {
                                Ok(true) => self.show_status("Import cancelled"),
                                Ok(false) => self
                                    .show_status(&format!("Session imported from: {input_path}")),
                                Err(error) => self.report_import_failure(error),
                            }
                        }
                        Some(crate::session::cwd::MissingSessionCwdChoice::Fork) => {
                            match self.switch_to_forked_session(&input_path, &issue.fallback_cwd) {
                                Ok(true) => self.show_status("Import cancelled"),
                                Ok(false) => {}
                                Err(error) => self.report_import_failure(error),
                            }
                        }
                        None => self.show_status("Import cancelled"),
                    }
                }
                (ImportDialog::MissingSessionCwd { .. }, ExtensionSelectorEvent::Cancel) => {
                    self.close_import_selector();
                    self.show_status("Import cancelled");
                }
            }
        }
    }

    /// `handleFatalRuntimeError("Failed to import session", error)`
    /// (`interactive-mode.ts:6798-6800`), minus the crash report and the
    /// exit: the port's session-switch failures are non-fatal status lines
    /// (see `switch_session`'s seam note), so the same message renders as an
    /// error line here.
    fn report_import_failure(&mut self, error: ImportFromJsonlError) {
        let message = match error {
            // Both variants render the message verbatim.
            ImportFromJsonlError::FileNotFound(message) | ImportFromJsonlError::Other(message) => {
                message
            }
            ImportFromJsonlError::MissingSessionCwd(_) => {
                // Unreachable: the missing-cwd case opens the prompt instead.
                return;
            }
        };
        self.show_error(&format!("Failed to import session: {message}"));
    }

    /// `AgentSessionRuntime.importFromJsonl` (`agent-session-runtime.ts:359-407`):
    /// copy the file into the session dir (deduplicated), gate on
    /// `session_before_switch`, verify the stored cwd, then swap the session.
    /// Returns `Ok(true)` when an extension cancelled the switch
    /// (`{ cancelled: true }`). Deviation (same seam as [`Self::switch_session`]):
    /// upstream rebuilds the whole runtime for the imported session's cwd;
    /// the port keeps the startup runtime and only swaps the session.
    fn import_from_jsonl(
        &mut self,
        input_path: &str,
        cwd_override: Option<&std::path::Path>,
    ) -> Result<bool, ImportFromJsonlError> {
        let resolved_path = crate::session::store::resolve_path(std::path::Path::new(input_path));
        if !resolved_path.exists() {
            return Err(ImportFromJsonlError::FileNotFound(format!(
                "File not found: {}",
                resolved_path.display()
            )));
        }

        let session_dir = self.session.get_session_dir().to_path_buf();
        if !session_dir.exists() {
            std::fs::create_dir_all(&session_dir)
                .map_err(|error| ImportFromJsonlError::Other(error.to_string()))?;
        }

        let mut destination_path = session_dir.join(
            resolved_path
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("session.jsonl")),
        );
        let source_already_stored =
            crate::session::store::paths_equal(&destination_path, &resolved_path);
        if !source_already_stored {
            // `${name}-${suffix++}${ext}` (`agent-session-runtime.ts:373-376`).
            let stem = destination_path.file_stem().map_or_else(
                || "session".to_string(),
                |stem| stem.to_string_lossy().into_owned(),
            );
            let extension = destination_path
                .extension()
                .map_or_else(String::new, |ext| format!(".{}", ext.to_string_lossy()));
            let mut suffix = 1;
            while destination_path.exists() {
                destination_path = session_dir.join(format!("{stem}-{suffix}{extension}"));
                suffix += 1;
            }
        }

        // `emitBeforeSwitch("resume", destinationPath)`
        // (`agent-session-runtime.ts:379-382`).
        let target = destination_path.to_string_lossy().into_owned();
        if self.extension_cancelled(&serde_json::json!({
            "type": "session_before_switch",
            "reason": "resume",
            "targetSessionFile": target,
        })) {
            return Ok(true);
        }

        if !source_already_stored {
            // `copyFileSync(resolvedPath, destinationPath, COPYFILE_EXCL)`.
            copy_file_exclusive(&resolved_path, &destination_path)
                .map_err(|error| ImportFromJsonlError::Other(error.to_string()))?;
        }

        let session =
            match SessionManager::open(&destination_path, Some(&session_dir), cwd_override) {
                Ok(session) => session,
                Err(error) => {
                    // The import did not happen; drop the copy so a retry with a
                    // cwd override or a fork leaves no stray file.
                    if !source_already_stored {
                        let _ = std::fs::remove_file(&destination_path);
                    }
                    return Err(ImportFromJsonlError::Other(error.to_string()));
                }
            };
        // `this.cwd` is the runtime's process cwd (`agent-session-runtime.ts:391`).
        let fallback_cwd =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
        if let Err(error) = crate::session::cwd::assert_session_cwd_exists(&session, &fallback_cwd)
        {
            if !source_already_stored {
                let _ = std::fs::remove_file(&destination_path);
            }
            return Err(ImportFromJsonlError::MissingSessionCwd(error.issue));
        }

        // `teardownCurrent("resume", …)` emits `session_shutdown` with the
        // destination before the replacement
        // (`agent-session-runtime.ts:396-399`).
        let destination_file = session
            .get_session_file()
            .map(|path| path.to_string_lossy().into_owned());
        self.emit_extension_shutdown("resume", destination_file.as_deref());
        self.swap_imported_session(session);
        Ok(false)
    }

    /// `switchToForkedSession` (`interactive-mode.ts:5982-5999`): fork the
    /// source into `cwd` (in the current session dir) and switch to it. The
    /// fork file is dropped when an extension vetoes the switch.
    fn switch_to_forked_session(
        &mut self,
        source_path: &str,
        cwd: &std::path::Path,
    ) -> Result<bool, ImportFromJsonlError> {
        let fork = SessionManager::fork_from(
            std::path::Path::new(source_path),
            cwd,
            Some(self.session.get_session_dir()),
            &NewSessionOptions::default(),
        )
        .map_err(|error| ImportFromJsonlError::Other(error.to_string()))?;
        let fork_path = fork
            .get_session_file()
            .expect("the fork writes its file eagerly")
            .to_path_buf();

        if self.extension_cancelled(&serde_json::json!({
            "type": "session_before_switch",
            "reason": "resume",
            "targetSessionFile": fork_path.to_string_lossy(),
        })) {
            // An extension vetoed the switch; drop the fork so cancelling
            // leaves no copy behind.
            let _ = std::fs::remove_file(&fork_path);
            return Ok(true);
        }

        self.emit_extension_shutdown("resume", Some(&fork_path.to_string_lossy()));
        let session_dir = Some(self.session.get_session_dir().to_path_buf());
        let session = match SessionManager::open(&fork_path, session_dir.as_deref(), None) {
            Ok(session) => session,
            Err(error) => {
                let _ = std::fs::remove_file(&fork_path);
                return Err(ImportFromJsonlError::Other(error.to_string()));
            }
        };
        let fork_id = session.get_session_id().to_string();
        self.swap_imported_session(session);
        self.show_status(&format!(
            "Forked into new session {fork_id} in {}",
            cwd.display()
        ));
        Ok(false)
    }

    /// `createRuntime` + `finishSessionReplacement` reduced to the port's
    /// session-swap seam (`agent-session-runtime.ts:399-406`): replay the
    /// new transcript and clear the in-flight state.
    fn swap_imported_session(&mut self, session: SessionManager) {
        self.session = session;
        self.active_prompt = None;
        self.queued_deliveries.clear();
        self.pending_tools.clear();
        self.streaming_index = None;
        self.compaction_run = None;
        self.render_initial_messages();
        self.refresh_footer();
    }

    // --------------------------------------------------------------
    // /fork and /clone (`showUserMessageSelector`,
    // `interactive-mode.ts:5692-5728`, `handleCloneCommand` `:5730-5749`,
    // and `AgentSessionRuntime.fork`, `agent-session-runtime.ts:262-350`)
    // --------------------------------------------------------------

    /// `contentText(content, "")` / `extractUserMessageText` for user
    /// content: the text parts joined with no separator
    /// (`agent-session-runtime.ts:56-65`).
    fn user_message_text(content: &pi_ai::types::UserContent) -> String {
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

    /// `getUserMessagesForForking` (`agent-session.ts:4342-4357`): every user
    /// message entry with non-empty text, in file order.
    fn user_messages_for_forking(&self) -> Vec<UserMessageItem> {
        let mut items = Vec::new();
        for entry in self.session.get_entries() {
            let crate::session::records::SessionEntry::Message(message) = entry else {
                continue;
            };
            let AgentMessage::User(user) = &message.message else {
                continue;
            };
            let text = Self::user_message_text(&user.content);
            if text.is_empty() {
                continue;
            }
            items.push(UserMessageItem {
                id: message.base.id.clone(),
                text,
            });
        }
        items
    }

    /// `showUserMessageSelector` (`interactive-mode.ts:5692-5728`): the fork
    /// dialog, preselected on the most recent message; a selection forks and
    /// puts the message text back into the editor.
    fn show_user_message_selector(&mut self) {
        if self.suspended_editor.is_some() {
            return;
        }
        let user_messages = self.user_messages_for_forking();
        if user_messages.is_empty() {
            self.show_status("No messages to fork from");
            return;
        }
        // `userMessages[userMessages.length - 1]?.entryId` (`:5700`).
        let initial_selected_id = user_messages.last().map(|message| message.id.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = UserMessageSelectorComponent::new(
            user_messages,
            initial_selected_id.as_deref(),
            Arc::clone(&events),
        );
        self.user_message_selector_events = Some(events);
        self.suspend_editor_with(Box::new(selector));
    }

    /// Put the editor back after the fork dialog closes.
    fn close_user_message_selector(&mut self) {
        if self.suspended_editor.is_none() {
            return;
        }
        self.user_message_selector_events = None;
        self.restore_editor();
    }

    /// The `/fork` dialog's events (`showUserMessageSelector`'s callbacks,
    /// `interactive-mode.ts:5705-5723`).
    fn handle_user_message_selector_event(&mut self, event: UserMessageSelectorEvent) {
        match event {
            UserMessageSelectorEvent::Selected(entry_id) => {
                self.close_user_message_selector();
                match self.fork_session(&entry_id, ForkPosition::Before) {
                    Ok(ForkOutcome::Cancelled) => self.tui.request_render(),
                    Ok(ForkOutcome::Completed(selected_text)) => {
                        // `this.editor.setText(result.selectedText ?? "")`
                        // (`:5714`): the forked session ends before the
                        // selected message, so its text returns for editing.
                        let text = selected_text.unwrap_or_default();
                        self.editor_mut().editor.set_text(&text);
                        self.show_status("Forked to new session");
                    }
                    Err(error) => self.show_error(&error),
                }
            }
            UserMessageSelectorEvent::Cancel => self.close_user_message_selector(),
        }
    }

    fn pump_user_message_selector(&mut self) {
        if self.user_message_selector_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .user_message_selector_events
                .as_ref()
                .expect("user message selector")
                .lock()
                .expect("user message selector events"),
        );
        for event in events {
            self.handle_user_message_selector_event(event);
        }
    }

    /// `/clone` (`handleCloneCommand`, `interactive-mode.ts:5730-5749`):
    /// duplicate the session at the current leaf.
    fn handle_clone_command(&mut self) {
        let Some(leaf_id) = self.session.get_leaf_id().map(str::to_string) else {
            self.show_status("Nothing to clone yet");
            return;
        };
        match self.fork_session(&leaf_id, ForkPosition::At) {
            Ok(ForkOutcome::Cancelled) => self.tui.request_render(),
            // `this.editor.setText("")` + "Cloned to new session" (`:5744-5745`).
            Ok(ForkOutcome::Completed(_)) => {
                self.editor_mut().editor.set_text("");
                self.show_status("Cloned to new session");
            }
            Err(error) => self.show_error(&error),
        }
    }

    /// `AgentSessionRuntime.fork` (`agent-session-runtime.ts:262-350`): copy
    /// the active path up to `entry_id` into a new session and swap to it.
    /// `Before` targets a user message (the new session ends at its parent,
    /// and the message text returns for editing); `At` ends at the entry
    /// itself. `Completed(Some(text))` carries the selected message's text
    /// for the editor (`selectedText`, `"before"` only).
    #[allow(clippy::too_many_lines)] // mirrors the TS AgentSessionRuntime.fork; split when extending
    fn fork_session(
        &mut self,
        entry_id: &str,
        position: ForkPosition,
    ) -> Result<ForkOutcome, String> {
        // `emitBeforeFork` (`agent-session-runtime.ts:150-166`): a `cancel`
        // vote aborts the fork.
        if self.extension_cancelled(&serde_json::json!({
            "type": "session_before_fork",
            "entryId": entry_id,
            "position": position.as_str(),
        })) {
            return Ok(ForkOutcome::Cancelled);
        }

        let Some(selected_entry) = self.session.get_entry(entry_id) else {
            return Err("Invalid entry ID for forking".to_string());
        };
        let selected_text;
        let target_leaf_id: Option<String>;
        match position {
            ForkPosition::At => {
                selected_text = None;
                target_leaf_id = Some(entry_id.to_string());
            }
            ForkPosition::Before => {
                let crate::session::records::SessionEntry::Message(message) = &selected_entry
                else {
                    return Err("Invalid entry ID for forking".to_string());
                };
                let AgentMessage::User(user) = &message.message else {
                    return Err("Invalid entry ID for forking".to_string());
                };
                selected_text = Some(Self::user_message_text(&user.content));
                target_leaf_id = selected_entry.parent_id().map(str::to_string);
            }
        }

        let session_dir = Some(self.session.get_session_dir().to_path_buf());
        if self.session.is_persisted() {
            let Some(current_session_file) = self
                .session
                .session_file()
                .map(|path| path.to_string_lossy().into_owned())
            else {
                return Err("Persisted session is missing a session file".to_string());
            };
            let Some(target_leaf_id) = target_leaf_id else {
                // `SessionManager.create(cwd, sessionDir)` +
                // `newSession({ parentSession })` (`:296-310`): forking from
                // the very first user message starts an empty session.
                let cwd = self.session.get_cwd().to_path_buf();
                let options = NewSessionOptions {
                    parent_session: Some(current_session_file),
                    ..NewSessionOptions::default()
                };
                let manager = SessionManager::create(&cwd, session_dir.as_deref(), &options)
                    .map_err(|error| error.to_string())?;
                // `teardownCurrent` settles the active response first
                // (`:299`), persisting it to the outgoing session.
                self.abort_active_run();
                self.fork_swap(manager);
                return Ok(ForkOutcome::Completed(selected_text));
            };
            if !std::path::Path::new(&current_session_file).exists() {
                return Err(
                    "This session has not been saved yet. Send a message before cloning or forking it."
                        .to_string(),
                );
            }
            // `SessionManager.open(currentSessionFile, sessionDir)` +
            // `createBranchedSession(targetLeafId)` (`:315-319`): the forked
            // copy is read before the teardown abort appends to the file,
            // like upstream.
            let mut forked = SessionManager::open(
                std::path::Path::new(&current_session_file),
                session_dir.as_deref(),
                None,
            )
            .map_err(|error| error.to_string())?;
            forked
                .create_branched_session(&target_leaf_id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Failed to create forked session".to_string())?;
            self.abort_active_run();
            self.fork_swap(forked);
            Ok(ForkOutcome::Completed(selected_text))
        } else {
            // The in-memory manager mutates in place (`:333-339`): teardown
            // first — abort, then `session_shutdown` (its file is
            // `undefined`, like upstream's non-persisted `getSessionFile()`)
            // — then a fresh session when forking from the very first user
            // message, otherwise the branched path.
            self.abort_active_run();
            self.emit_extension_shutdown("fork", None);
            if let Some(target_leaf_id) = target_leaf_id {
                self.session
                    .create_branched_session(&target_leaf_id)
                    .map_err(|error| error.to_string())?;
            } else {
                let options = NewSessionOptions {
                    parent_session: self
                        .session
                        .session_file()
                        .map(|path| path.to_string_lossy().into_owned()),
                    ..NewSessionOptions::default()
                };
                self.session
                    .new_session(&options)
                    .map_err(|error| error.to_string())?;
            }
            self.after_session_store_swap();
            Ok(ForkOutcome::Completed(selected_text))
        }
    }

    /// The persisted fork's swap tail: `session_shutdown` with the
    /// destination (`teardownCurrent`, `agent-session-runtime.ts:167-175`),
    /// then the store swap, replay, and footer refresh.
    fn fork_swap(&mut self, session: SessionManager) {
        let destination = session
            .session_file()
            .map(|path| path.to_string_lossy().into_owned());
        self.emit_extension_shutdown("fork", destination.as_deref());
        self.session = session;
        self.after_session_store_swap();
    }

    /// The store-swap tail shared by the fork paths: drop the run state, then
    /// replay the new store's transcript (upstream re-renders inside the
    /// runtime replacement; the port mirrors `switch_session`'s tail).
    fn after_session_store_swap(&mut self) {
        self.active_prompt = None;
        self.queued_deliveries.clear();
        self.pending_tools.clear();
        self.streaming_index = None;
        self.render_initial_messages();
        self.refresh_footer();
    }

    // --------------------------------------------------------------
    // /tree (`showTreeSelector`, `interactive-mode.ts:5751-5898`, and
    // `AgentSession.navigateTree`, `agent-session.ts:4140-4337`)
    // --------------------------------------------------------------

    /// Whether compaction or a branch summary is running
    /// (`session.isCompacting`, `agent-session.ts:1719-1726`).
    fn is_compacting(&self) -> bool {
        self.compaction_run.is_some()
            || self
                .tree_navigation
                .as_ref()
                .is_some_and(|navigation| navigation.summary_run.is_some())
    }

    /// `showTreeSelector` (`interactive-mode.ts:5751-5898`): the tree dialog
    /// over the session's entries, preselected on the current leaf (or the
    /// re-open target), seeded with the persisted filter mode.
    fn show_tree_selector(&mut self, initial_selected_id: Option<&str>) {
        if self.suspended_editor.is_some() {
            return;
        }
        let tree = self.session.get_tree();
        // `this.sessionManager.getTree()` + `getLeafId()` (`:5752-5753`).
        let real_leaf_id = self.session.get_leaf_id().map(str::to_string);
        let initial_filter_mode = self.settings.get_tree_filter_mode();
        if tree.is_empty() {
            self.show_status("No entries in session");
            return;
        }
        // `this.ui.terminal.rows` → `maxVisibleLines`
        // (`tree-selector.ts:1368`).
        let terminal_rows = self
            .tui
            .terminal()
            .size()
            .map_or(24, |size| usize::from(size.rows));
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = TreeSelectorComponent::new(
            &tree,
            real_leaf_id.as_deref(),
            terminal_rows,
            initial_selected_id,
            initial_filter_mode,
            Arc::clone(&events),
        );
        self.tree_selector_events = Some(events);
        self.suspend_editor_with(Box::new(selector));
    }

    /// Put the editor back after the tree dialog closes.
    fn close_tree_selector(&mut self) {
        if self.suspended_editor.is_none() {
            return;
        }
        self.tree_selector_events = None;
        self.restore_editor();
    }

    /// The `/tree` dialog's events (`showTreeSelector`'s callbacks,
    /// `interactive-mode.ts:5766-5895`).
    fn handle_tree_selector_event(&mut self, event: TreeSelectorEvent) {
        match event {
            TreeSelectorEvent::Selected(entry_id) => {
                self.close_tree_selector();
                self.handle_tree_selection(entry_id);
            }
            TreeSelectorEvent::Cancel => self.close_tree_selector(),
            TreeSelectorEvent::Copy(text) => match text {
                Some(text) => match crate::clipboard::copy_to_clipboard(&text) {
                    Ok(()) => self.show_status("Copied selected message to clipboard"),
                    Err(error) => self.show_error(&error),
                },
                // `if (!text)` (`:5885-5888`).
                None => self.show_error("Selected entry has no text to copy"),
            },
            TreeSelectorEvent::LabelChange(entry_id, label) => {
                // `(entryId, label) => { this.sessionManager
                // .appendLabelChange(entryId, label); this.ui.requestRender(); }`
                // (`:5877-5880`).
                if let Err(error) = self
                    .session
                    .append_label_change(&entry_id, label.as_deref())
                {
                    self.show_error(&error.to_string());
                }
                self.tui.request_render();
            }
        }
    }

    fn pump_tree_selector(&mut self) {
        if self.tree_selector_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .tree_selector_events
                .as_ref()
                .expect("tree selector")
                .lock()
                .expect("tree selector events"),
        );
        for event in events {
            self.handle_tree_selector_event(event);
        }
    }

    /// The selection tail (`interactive-mode.ts:5766-5808`): the current-leaf
    /// no-op, then the summarize prompt unless `branchSummary.skipPrompt`.
    fn handle_tree_selection(&mut self, entry_id: String) {
        // Selecting the current leaf is a no-op (already there).
        if self.session.get_leaf_id() == Some(entry_id.as_str()) {
            self.show_status("Already at this point");
            return;
        }

        if !self.settings.get_branch_summary_settings().skip_prompt {
            // Loop until the user makes a complete choice or cancels to tree
            // (`:5777-5808`): the port stages the target and mounts the
            // dialog; the pumps drive the loop.
            self.tree_navigation = Some(TreeNavigationRun {
                target_id: entry_id,
                summarize: false,
                custom_instructions: None,
                summary_run: None,
            });
            self.open_tree_summary_dialog();
            return;
        }
        self.proceed_tree_navigation(entry_id, false, None);
    }

    /// The "Summarize branch?" dialog (`showExtensionSelector`'s built-in
    /// arm, `:5783-5788`).
    fn open_tree_summary_dialog(&mut self) {
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = ExtensionSelectorComponent::new(
            "Summarize branch?",
            vec![
                "No summary".to_string(),
                "Summarize".to_string(),
                "Summarize with custom prompt".to_string(),
            ],
            Arc::clone(&events),
        );
        self.tree_summary_dialog_events = Some(events);
        self.suspend_editor_with(Box::new(selector));
    }

    fn handle_tree_summary_event(&mut self, event: ExtensionSelectorEvent) {
        // The navigation under the dialog: restore the editor, then drive
        // the decision loop.
        self.restore_editor();
        self.tree_summary_dialog_events = None;
        let Some(navigation) = self.tree_navigation.take() else {
            return;
        };
        match event {
            ExtensionSelectorEvent::Cancel => {
                // User pressed escape — re-show the tree selector with the
                // same selection (`:5790-5794`).
                self.show_tree_selector(Some(&navigation.target_id));
            }
            ExtensionSelectorEvent::Selected(choice) => {
                let mut navigation = navigation;
                if choice == "Summarize with custom prompt" {
                    // `customInstructions = await this.showExtensionEditor(...)`
                    // (`:5798-5803`): a cancel loops back to the summary
                    // dialog.
                    navigation.summarize = true;
                    self.tree_navigation = Some(navigation);
                    self.open_tree_prompt_editor();
                    return;
                }
                navigation.summarize = choice == "Summarize";
                let TreeNavigationRun {
                    target_id,
                    summarize,
                    custom_instructions,
                    ..
                } = navigation;
                self.proceed_tree_navigation(target_id, summarize, custom_instructions);
            }
        }
    }

    fn pump_tree_summary_dialog(&mut self) {
        if self.tree_summary_dialog_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .tree_summary_dialog_events
                .as_ref()
                .expect("tree summary dialog")
                .lock()
                .expect("tree summary dialog events"),
        );
        for event in events {
            self.handle_tree_summary_event(event);
        }
    }

    /// The custom-summarization-prompt editor (`showExtensionEditor`,
    /// `:5799`).
    fn open_tree_prompt_editor(&mut self) {
        if self.suspended_editor.is_some() {
            return;
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let selector = ExtensionEditorComponent::new(
            "Custom summarization instructions",
            None,
            Arc::clone(&events),
        );
        self.tree_prompt_editor_events = Some(events);
        self.suspend_editor_with(Box::new(selector));
    }

    fn handle_tree_prompt_event(&mut self, event: ExtensionEditorEvent) {
        self.restore_editor();
        self.tree_prompt_editor_events = None;
        let Some(navigation) = self.tree_navigation.take() else {
            return;
        };
        match event {
            ExtensionEditorEvent::Cancel => {
                // A cancelled editor loops back to the summary selector
                // (`:5800-5804`).
                self.tree_navigation = Some(navigation);
                self.open_tree_summary_dialog();
            }
            ExtensionEditorEvent::Submitted(text) => {
                let TreeNavigationRun { target_id, .. } = navigation;
                // `customInstructions = await this.showExtensionEditor(...)`
                // (`:5799`): the submitted text rides verbatim.
                self.proceed_tree_navigation(target_id, true, Some(text));
            }
        }
    }

    fn pump_tree_prompt_editor(&mut self) {
        if self.tree_prompt_editor_events.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .tree_prompt_editor_events
                .as_ref()
                .expect("tree prompt editor")
                .lock()
                .expect("tree prompt editor events"),
        );
        for event in events {
            self.handle_tree_prompt_event(event);
        }
    }

    /// The committed navigation (`interactive-mode.ts:5811-5837`): stop the
    /// active response, recheck availability, run the `session_before_tree`
    /// gate, then start the summarizer or navigate directly.
    fn proceed_tree_navigation(
        &mut self,
        target_id: String,
        summarize: bool,
        custom_instructions: Option<String>,
    ) {
        // The user committed to navigating: stop the active response first.
        if self.agent_run.is_some() {
            self.dequeue_queued();
            self.abort_active_run();
        }
        // Recheck after the dialogs and streaming abort, before replacing
        // another operation's UI.
        if self.is_compacting() {
            self.show_error(
                "Wait for the current compaction or tree navigation to finish before navigating the session tree.",
            );
            return;
        }

        // The `session_before_tree` gate (`agent-session.ts:4201-4211`).
        // Upstream runs it inside navigateTree, ahead of the summarization;
        // the port hoists it ahead of the worker thread for the same
        // ordering. The preparation rides verbatim minus `label` (the
        // extension overrides land with E3).
        let old_leaf_id = self.session.get_leaf_id().map(str::to_string);
        let (entries_to_summarize, common_ancestor_id) =
            crate::session::branch_summarization::collect_entries_for_branch_summary(
                &self.session,
                old_leaf_id.as_deref(),
                &target_id,
            );
        let preparation = serde_json::json!({
            "targetId": target_id,
            "oldLeafId": old_leaf_id,
            "commonAncestorId": common_ancestor_id,
            "userWantsSummary": summarize,
            "customInstructions": custom_instructions,
            "replaceInstructions": false,
            "entriesToSummarize": entries_to_summarize
                .iter()
                .map(|entry| serde_json::Value::Object(entry.to_record().object().clone()))
                .collect::<Vec<_>>(),
        });
        if self.extension_cancelled(&serde_json::json!({
            "type": "session_before_tree",
            "preparation": preparation,
        })) {
            self.show_status("Navigation cancelled");
            return;
        }

        self.tree_navigation = Some(TreeNavigationRun {
            target_id,
            summarize,
            custom_instructions,
            summary_run: None,
        });
        if summarize {
            self.start_branch_summary();
        } else {
            let navigation = self.tree_navigation.take().expect("navigation");
            self.finish_tree_navigation(&navigation, None);
        }
    }

    /// The branch summary worker (`navigateTree`'s summarize phase,
    /// `agent-session.ts:4230-4263`): the entries between the old leaf and
    /// the target go to `generateBranchSummary` off the UI thread, with the
    /// `BranchSummaryStatusIndicator` standing in (`:5825-5837`).
    fn start_branch_summary(&mut self) {
        let (target_id, custom_instructions) = {
            let Some(navigation) = &self.tree_navigation else {
                return;
            };
            (
                navigation.target_id.clone(),
                navigation.custom_instructions.clone(),
            )
        };
        let old_leaf_id = self.session.get_leaf_id().map(str::to_string);
        let (entries_to_summarize, _common_ancestor_id) =
            crate::session::branch_summarization::collect_entries_for_branch_summary(
                &self.session,
                old_leaf_id.as_deref(),
                &target_id,
            );
        if entries_to_summarize.is_empty() {
            // `options.summarize && entriesToSummarize.length > 0`
            // (`:4234`): an empty branch navigates without a summary.
            let navigation = self.tree_navigation.take().expect("navigation");
            self.finish_tree_navigation(&navigation, None);
            return;
        }

        let model = self.model.clone();
        let (api_key, access_token) = self.run_wire_credential();
        let thinking_level = self.thinking_level;
        let stream_fn = Arc::clone(&self.stream_fn);
        let reserve_tokens = self
            .settings
            .get_branch_summary_settings()
            .reserve_tokens
            .max(0) as u64;
        let signal = Arc::new(AbortSignal::new());
        let worker_signal = Arc::clone(&signal);
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let options = crate::session::branch_summarization::BranchSummaryOptions {
                model: &model,
                api_key: api_key.as_deref(),
                access_token: access_token.as_deref(),
                signal: &worker_signal,
                thinking_level,
                custom_instructions: custom_instructions.as_deref(),
                replace_instructions: false,
                reserve_tokens,
                stream_fn: stream_fn.as_ref(),
            };
            let result = crate::runtime::block_on(
                crate::session::branch_summarization::generate_branch_summary(
                    &entries_to_summarize,
                    &options,
                ),
            );
            let _ = tx.send(result);
        });
        if let Some(navigation) = self.tree_navigation.as_mut() {
            navigation.summary_run = Some(BranchSummaryRun {
                abort: signal,
                events: rx,
                handle: Some(handle),
            });
        }
        // The status indicator rides the editor border, like upstream's
        // `BranchSummaryStatusIndicator` (`status-indicator.ts:102-112`).
        self.set_working_status(Some(StatusIndicator::compaction(&format!(
            "Summarizing branch... ({} to cancel)",
            theme::key_text(theme::app_keys::INTERRUPT)
        ))));
        self.tui.request_render();
    }

    fn pump_branch_summary(&mut self) {
        let Some(receiver) = self
            .tree_navigation
            .as_ref()
            .and_then(|navigation| navigation.summary_run.as_ref())
            .map(|run| &run.events)
        else {
            return;
        };
        let Ok(result) = receiver.try_recv() else {
            return;
        };
        // Join the worker before mutating state (`abort_active_run`'s tail).
        if let Some(run) = self
            .tree_navigation
            .as_mut()
            .and_then(|navigation| navigation.summary_run.take())
            && let Some(handle) = run.handle
        {
            let _ = handle.join();
        }
        // `clearStatusIndicator("branchSummary")` (`:5866-5870`).
        self.set_working_status(None);

        if result.aborted {
            // Summarization aborted — re-show the tree selector with the
            // same selection (`:5845-5850`).
            let target = self
                .tree_navigation
                .take()
                .map(|navigation| navigation.target_id)
                .unwrap_or_default();
            self.show_status("Branch summarization cancelled");
            self.show_tree_selector(Some(&target));
            return;
        }
        if let Some(error) = result.error {
            self.tree_navigation = None;
            self.show_error(&error);
            return;
        }
        let Some(navigation) = self.tree_navigation.take() else {
            return;
        };
        self.finish_tree_navigation(&navigation, Some(result));
    }

    /// `navigateTree` + the UI update (`agent-session.ts:4140-4337`,
    /// `interactive-mode.ts:5839-5871`): the gate and the LLM phase already
    /// ran (`proceed_tree_navigation` + the worker), so this is the leaf
    /// move, the `session_tree` emission, and the transcript replay.
    fn finish_tree_navigation(
        &mut self,
        navigation: &TreeNavigationRun,
        summary: Option<crate::session::branch_summarization::BranchSummaryResult>,
    ) {
        let summary = summary.and_then(|result| {
            let summary_text = result.summary?;
            // `summaryDetails = { readFiles, modifiedFiles }` (`:4255-4258`).
            let details = serde_json::json!({
                "readFiles": result.read_files,
                "modifiedFiles": result.modified_files,
            });
            Some((summary_text, details, result.usage))
        });
        match self.navigate_tree(&navigation.target_id, summary) {
            Ok(editor_text) => {
                // Update UI: the transcript replays from the new leaf
                // (`:5856-5863`).
                self.chat_mut().clear();
                self.last_status_pair = None;
                self.render_initial_messages();
                if let Some(text) = editor_text
                    && self.editor_mut().editor.get_text().trim().is_empty()
                {
                    self.editor_mut().editor.set_text(&text);
                }
                self.show_status("Navigated to selected point");
                self.tui.request_render();
            }
            Err(error) => self.show_error(&error),
        }
    }

    /// `AgentSession.navigateTree` (`agent-session.ts:4140-4337`) without the
    /// LLM summarization (the worker above) and with the gate hoisted to the
    /// call site: the guards, the leaf move, the summary attachment, and the
    /// `session_tree` emission.
    fn navigate_tree(
        &mut self,
        target_id: &str,
        summary: Option<(String, serde_json::Value, Option<pi_ai::types::Usage>)>,
    ) -> Result<Option<String>, String> {
        if self.agent_run.is_some() {
            return Err(
                "Wait for the current response to finish before navigating the session tree."
                    .to_string(),
            );
        }
        if self.is_compacting() {
            return Err(
                "Wait for the current compaction or tree navigation to finish before navigating the session tree."
                    .to_string(),
            );
        }

        let old_leaf_id = self.session.get_leaf_id().map(str::to_string);

        // No-op if already at target (`:4155-4158`).
        if old_leaf_id.as_deref() == Some(target_id) {
            return Ok(None);
        }

        let Some(target_entry) = self.session.get_entry(target_id) else {
            return Err(format!("Entry {target_id} not found"));
        };

        // The `session_before_tree` gate ran at the single call site,
        // ahead of the summarizer worker (`:4201-4211`); the collected
        // entries and preparation live there.

        // Determine the new leaf position based on the target type
        // (`:4265-4280`).
        let (new_leaf_id, editor_text) = match &target_entry {
            crate::session::records::SessionEntry::Message(message) => match &message.message {
                AgentMessage::User(user) => (
                    message.base.parent_id.clone(),
                    Some(Self::user_message_text(&user.content)),
                ),
                _ => (Some(target_id.to_string()), None),
            },
            crate::session::records::SessionEntry::CustomMessage(custom) => {
                let text = match &custom.content {
                    pi_ai::types::UserContent::Text(text) => text.clone(),
                    pi_ai::types::UserContent::Blocks(blocks) => blocks
                        .iter()
                        .filter_map(|block| match block {
                            pi_ai::types::InputContent::Text(text) => Some(text.text.clone()),
                            pi_ai::types::InputContent::Image(_) => None,
                        })
                        .collect::<String>(),
                };
                (custom.base.parent_id.clone(), Some(text))
            }
            _ => (Some(target_id.to_string()), None),
        };

        // Switch leaf, with or without the summary (`:4282-4306`): the
        // summary attaches at the navigation target position, not the old
        // branch.
        let mut summary_entry_id: Option<String> = None;
        if let Some((summary_text, details, usage)) = summary {
            let summary_id = self
                .session
                .branch_with_summary(
                    new_leaf_id.as_deref(),
                    &summary_text,
                    Some(details),
                    Some(false),
                    usage,
                )
                .map_err(|error| error.to_string())?;
            summary_entry_id = Some(summary_id);
        } else if new_leaf_id.is_none() {
            // Navigating to root — reset the leaf.
            self.session.reset_leaf();
        } else {
            self.session
                .branch(new_leaf_id.as_deref().expect("new leaf"))
                .map_err(|error| error.to_string())?;
        }

        // The port keeps the run state on the mode, not the session store:
        // a navigation abandons any pending tool rows and streaming view.
        self.pending_tools.clear();
        self.streaming_index = None;

        // `session_tree` (`:4319-4325`).
        self.emit_extension_event(&serde_json::json!({
            "type": "session_tree",
            "newLeafId": self.session.get_leaf_id(),
            "oldLeafId": old_leaf_id,
            "summaryEntry": summary_entry_id,
        }));

        Ok(editor_text)
    }

    /// A notification-only extension emit (`runner.emit`): errors surface as
    /// warnings, like `emit_extension_shutdown`.
    fn emit_extension_event(&self, event: &serde_json::Value) {
        #[cfg(feature = "ext-js")]
        {
            let Some(runner) = &self.extension_runner else {
                return;
            };
            if let Err(error) = crate::extensions::emit_gate(runner, event) {
                let event_name = event
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                eprintln!("Warning: Extension error: {event_name}: {error}");
            }
        }
        #[cfg(not(feature = "ext-js"))]
        {
            let _ = event;
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
            // Extension oauth providers run through the engine bridge
            // (`adaptOAuth`'s login arm, `provider-composer.ts:363-375`).
            #[cfg(feature = "ext-js")]
            if self.start_extension_oauth_login(provider, back) {
                return;
            }
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

    // ---- Extension oauth login (slice 6j: `showLoginDialog`/`loginProvider`'s
    // `"oauth"` arm, `interactive-mode.ts:6539-6583`, driving the engine
    // bridge — `adaptOAuth`'s login, `provider-composer.ts:363-375`) ----

    /// Starts the extension-oauth login flow when the provider registered
    /// one: `false` leaves the caller to the built-in paths (the upstream
    /// error for the unported per-provider flows).
    #[cfg(feature = "ext-js")]
    fn start_extension_oauth_login(
        &mut self,
        provider: &AuthSelectorProvider,
        back: Option<LoginBackStep>,
    ) -> bool {
        let started = self.extension_runner.as_ref().map(|runner| {
            let mut runner = runner.lock().expect("extension runner mutex poisoned");
            if !runner.oauth_has_login(&provider.id) {
                return None;
            }
            Some(runner.oauth_login_start(&provider.id))
        });
        match started {
            None | Some(None) => return false,
            Some(Some(Err(error))) => {
                self.show_error(&format!("Failed to login to {}: {error}", provider.name));
                return true;
            }
            Some(Some(Ok(session))) => {
                self.extension_login_flow = Some(ExtensionLoginFlow {
                    session,
                    provider_id: provider.id.clone(),
                    provider_name: provider.name.clone(),
                    pending_prompt: None,
                    back,
                });
            }
        }
        self.pump_extension_oauth_login();
        true
    }

    /// The flow's engine poll (`loginProvider` awaiting the runtime login):
    /// drain notifications, mount the pending ask, settle the credential.
    /// The rich `LoginDialogComponent` presentation (`login-dialog.ts`)
    /// lands with the E3 TUI slice — the flow's notifications surface as
    /// status lines and the asks reuse the prompt/select components
    /// (HANDOFF deviation, presentation only).
    #[cfg(feature = "ext-js")]
    fn pump_extension_oauth_login(&mut self) {
        let Some(mut flow) = self.extension_login_flow.take() else {
            return;
        };
        let poll = match self.extension_runner.as_ref().map(|runner| {
            runner
                .lock()
                .expect("extension runner mutex poisoned")
                .oauth_login_poll(flow.session)
        }) {
            Some(Ok(poll)) => poll,
            Some(Err(error)) => {
                self.finish_extension_login_error(&flow, &error.to_string());
                return;
            }
            None => {
                self.extension_login_flow = Some(flow);
                return;
            }
        };
        for notification in &poll.notifications {
            self.show_extension_login_notification(&flow, notification);
        }
        if let Some((token, payload)) = poll.prompt {
            if self.mount_extension_login_ask(&mut flow, token, &payload) {
                self.extension_login_flow = Some(flow);
            }
            return;
        }
        if let Some(settled) = poll.settled {
            match settled {
                Ok(credential) => self.finish_extension_login(&flow, credential),
                Err(message) => self.finish_extension_login_error(&flow, &message),
            }
            return;
        }
        // Still running with nothing new this tick — the pump calls again.
        self.extension_login_flow = Some(flow);
    }

    /// `notifyAuthDialog` (`interactive-mode.ts:6507-6518`) as status lines:
    /// the sign-in URL (with its instructions), the device code, the
    /// progress text.
    #[cfg(feature = "ext-js")]
    fn show_extension_login_notification(
        &mut self,
        flow: &ExtensionLoginFlow,
        notification: &serde_json::Value,
    ) {
        match notification.get("type").and_then(serde_json::Value::as_str) {
            Some("auth_url") => {
                if let Some(instructions) = notification
                    .get("instructions")
                    .and_then(serde_json::Value::as_str)
                {
                    self.show_status(instructions);
                }
                let url = notification
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                self.show_status(&format!("Sign in: {url}"));
            }
            Some("device_code") => {
                let user_code = notification
                    .get("userCode")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let verification_uri = notification
                    .get("verificationUri")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                self.show_status(&format!("Enter code {user_code} at {verification_uri}"));
                self.show_status(&format!(
                    "Waiting for authentication... ({})",
                    flow.provider_name
                ));
            }
            // `progress` and anything else the bridge forwards.
            _ => {
                if let Some(message) = notification
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                {
                    self.show_status(message);
                }
            }
        }
    }

    /// Mounts the ask's dialog (`showAuthPrompt`,
    /// `interactive-mode.ts:6483-6505`): a select opens the option
    /// selector, text/manual-code opens the entry dialog. Returns `false`
    /// when the ask was unknown and the flow finished instead.
    #[cfg(feature = "ext-js")]
    fn mount_extension_login_ask(
        &mut self,
        flow: &mut ExtensionLoginFlow,
        token: u64,
        payload: &serde_json::Value,
    ) -> bool {
        match payload.get("type").and_then(serde_json::Value::as_str) {
            Some("select") => {
                let message = payload
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Select an option");
                let labels: Vec<String> = payload
                    .get("options")
                    .and_then(serde_json::Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(|option| {
                                option
                                    .get("label")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let events = Arc::new(Mutex::new(Vec::new()));
                let selector = AuthSelectorComponent::options(message, labels, Arc::clone(&events));
                self.extension_login_select = Some(events);
                flow.pending_prompt = Some((token, payload.clone()));
                self.suspend_editor_with(Box::new(selector));
                true
            }
            Some("text" | "manual_code") => {
                let message = payload
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Enter value");
                let placeholder = payload
                    .get("placeholder")
                    .and_then(serde_json::Value::as_str);
                let events = Arc::new(Mutex::new(Vec::new()));
                let prompt = LoginPromptComponent::new(message, placeholder, Arc::clone(&events));
                self.extension_login_prompt = Some(events);
                flow.pending_prompt = Some((token, payload.clone()));
                self.suspend_editor_with(Box::new(prompt));
                true
            }
            _ => {
                let kind = payload
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                self.finish_extension_login_error(
                    flow,
                    &format!("Login prompt type {kind:?} is not supported"),
                );
                false
            }
        }
    }

    /// Resolves the flow's pending ask and keeps polling
    /// (`oauth_login_answer` + the next drain); errors finish the flow.
    #[cfg(feature = "ext-js")]
    fn answer_extension_login_prompt(&mut self, answer: &serde_json::Value) {
        let (session, token) = match self.extension_login_flow.as_ref() {
            Some(flow) => match flow.pending_prompt {
                Some((token, _)) => (flow.session, token),
                None => return,
            },
            None => return,
        };
        let answered = self.extension_runner.as_ref().map(|runner| {
            runner
                .lock()
                .expect("extension runner mutex poisoned")
                .oauth_login_answer(session, token, answer)
        });
        match answered {
            Some(Ok(())) => {
                if let Some(flow) = self.extension_login_flow.as_mut() {
                    flow.pending_prompt = None;
                }
                self.pump_extension_oauth_login();
            }
            Some(Err(error)) => {
                if let Some(flow) = self.extension_login_flow.take() {
                    self.finish_extension_login_error(&flow, &error.to_string());
                }
            }
            // No engine: the flow cannot continue.
            None => self.extension_login_flow = None,
        }
    }

    /// Escape from a mounted ask (`Login cancelled` — the dialog's abort
    /// signal, `interactive-mode.ts:6441-6443`): kill the engine session
    /// and return to the `onBack` target. The abort flips the adapter
    /// `signal` the handler polls; the session ends regardless, so the
    /// abort is the host-side cancel rather than a cooperative stop.
    #[cfg(feature = "ext-js")]
    fn cancel_extension_login_flow(&mut self) {
        let Some(flow) = self.extension_login_flow.take() else {
            self.tui.request_render();
            return;
        };
        if let Some(runner) = self.extension_runner.as_ref() {
            let mut runner = runner.lock().expect("extension runner mutex poisoned");
            let _ = runner.oauth_login_abort(flow.session);
            let _ = runner.oauth_login_end(flow.session);
        }
        let back = flow.back;
        self.run_login_back(back);
    }

    /// The extension-login text prompt's drain (`showPrompt`'s resolver).
    #[cfg(feature = "ext-js")]
    fn pump_extension_login_prompt(&mut self) {
        if self.extension_login_prompt.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .extension_login_prompt
                .as_ref()
                .expect("extension login prompt")
                .lock()
                .expect("extension login prompt events"),
        );
        for event in events {
            match event {
                LoginPromptEvent::Submit(value) => {
                    self.extension_login_prompt = None;
                    self.restore_editor();
                    self.answer_extension_login_prompt(&serde_json::Value::String(value));
                }
                LoginPromptEvent::Cancel => {
                    self.extension_login_prompt = None;
                    self.restore_editor();
                    self.cancel_extension_login_flow();
                }
            }
        }
    }

    /// The extension-login select's drain (`showAuthSelect`'s resolver,
    /// `interactive-mode.ts:6450-6481`): the picked label maps back to its
    /// option id; an unknown label or escape is `Login cancelled`.
    #[cfg(feature = "ext-js")]
    fn pump_extension_login_select(&mut self) {
        if self.extension_login_select.is_none() {
            return;
        }
        let events = std::mem::take(
            &mut *self
                .extension_login_select
                .as_ref()
                .expect("extension login select")
                .lock()
                .expect("extension login select events"),
        );
        for event in events {
            match event {
                AuthSelectorEvent::SelectOption(label) => {
                    self.extension_login_select = None;
                    self.restore_editor();
                    let answer = self
                        .extension_login_flow
                        .as_ref()
                        .and_then(|flow| flow.pending_prompt.as_ref().map(|(_, payload)| payload))
                        .and_then(|payload| {
                            payload.get("options").and_then(serde_json::Value::as_array)
                        })
                        .and_then(|options| {
                            options
                                .iter()
                                .find(|option| {
                                    option.get("label").and_then(serde_json::Value::as_str)
                                        == Some(label.as_str())
                                })
                                .and_then(|option| option.get("id").cloned())
                        });
                    match answer {
                        Some(id) => self.answer_extension_login_prompt(&id),
                        None => self.cancel_extension_login_flow(),
                    }
                }
                AuthSelectorEvent::Cancel => {
                    self.extension_login_select = None;
                    self.restore_editor();
                    self.cancel_extension_login_flow();
                }
                // Option rows never publish `Select`.
                AuthSelectorEvent::Select { .. } => {}
            }
        }
    }

    /// The extension `ctx.ui` bridge's pump (E3 slices 6a+6b): drains the
    /// runner's engine-side request queue, then the bridge channel, then
    /// the dialog stack's deadlines and mailboxes. Dialog requests mount
    /// onto the stack immediately (`pushExtensionDialog`,
    /// `interactive-mode.ts:2833-2838`) — a new dialog hides the visible
    /// one and closing the visible one shows the previous again.
    #[cfg(feature = "ext-js")]
    #[allow(clippy::too_many_lines)] // mirrors the TS …; split when extending
    fn pump_extension_ui(&mut self) {
        // The runner drains the engine's ui_requests: a custom done()
        // crossing forwards as fire-and-forget and ends its ui_prompt
        // pair; nested dialog requests come back for delivery on threads.
        // Those threads begin the dialog under a guard, wait for the
        // answer OUTSIDE it (`begin_ui_dialog`), and complete under a
        // fresh guard — upstream's pending dialogs hold nothing but the
        // JS dispatch they started in, and this thread's crossings
        // (component render/input) need the mutex while a dialog shows.
        // This stays a try_lock for the frames where a competing JS run
        // (an emit mid-settle, a dialog completion) briefly holds the
        // mutex; skipping a frame beats stalling the UI thread. The Arc is
        // cloned first so the guard's destructor never holds a borrow of
        // `self` across the show_error arm.
        let runner_arc = self.extension_runner.as_ref().map(Arc::clone);
        if let Some(runner_arc) = runner_arc
            && let Ok(mut runner) = runner_arc.try_lock()
        {
            let service = runner.service_ui_requests();
            match service {
                Ok(deferred) => {
                    for request in deferred {
                        let runner = Arc::clone(&runner_arc);
                        std::thread::spawn(move || {
                            let wait = match runner
                                .lock()
                                .expect("extension runner mutex poisoned")
                                .begin_ui_dialog(&request)
                            {
                                Ok(Some(wait)) => wait,
                                Ok(None) => return,
                                Err(err) => {
                                    eprintln!("Warning: extension dialog delivery failed: {err}");
                                    return;
                                }
                            };
                            let response = wait.wait();
                            if let Err(err) = runner
                                .lock()
                                .expect("extension runner mutex poisoned")
                                .complete_ui_dialog(&wait, &response)
                            {
                                eprintln!("Warning: extension dialog completion failed: {err}");
                            }
                        });
                    }
                }
                Err(err) => self.show_error(&err.to_string()),
            }
        }
        let mut requests: Vec<serde_json::Value> = Vec::new();
        if let Some(bridge) = self.extension_ui_bridge.as_ref() {
            while let Some(request) = bridge.try_request() {
                requests.push(request);
            }
        }
        for request in requests {
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            match method.as_str() {
                // done() closed a custom dialog: the glue resolved its
                // promise, the close only disposes the component and
                // re-shows what was under it (`removeExtensionDialog`).
                // An overlay custom closes through `removeOverlay`
                // (`showExtensionCustom`'s `close`, `interactive-mode.ts
                // :3122-3133`) instead of the dialog stack.
                "customDone" => {
                    let id = request
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if self.extension_overlays.iter().any(|entry| entry.id == id) {
                        self.remove_extension_overlay(&id);
                    } else {
                        self.close_extension_ui_dialog_by_id(&id, &serde_json::json!({}));
                    }
                }
                // The overlay arm of `showExtensionCustom`
                // (`interactive-mode.ts:3139-3157`): `blocked` rides the
                // request, so the mount reports it. It must sit ahead of the
                // plain `custom` arm.
                "custom" if request.get("overlay").and_then(Value::as_bool) == Some(true) => {
                    let blocked = request
                        .get("blocked")
                        .filter(|value| !value.is_null())
                        .and_then(Self::blocked_status_from_json);
                    self.mount_extension_overlay(&request, "overlayOptions", blocked);
                }
                "select" | "confirm" | "input" | "editor" | "custom" => {
                    if let Some(entry) = self.extension_dialog_entry(&request) {
                        self.push_extension_dialog_entry(entry);
                    }
                }
                // ---- E3 slice 7: the small ctx.ui surfaces ----
                // `setExtensionWidget` (`interactive-mode.ts:2538-2578`):
                // lines, an adopted factory component, or removal — the
                // key spans both placements.
                "setWidget" => {
                    let key = request
                        .get("widgetKey")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let placement = request
                        .get("widgetPlacement")
                        .and_then(Value::as_str)
                        .unwrap_or("aboveEditor")
                        .to_string();
                    let content = request
                        .get("widgetHandle")
                        .and_then(Value::as_u64)
                        .map(|handle| ExtensionWidget::Adopted {
                            handle: handle as u32,
                            has_input: true,
                        })
                        .or_else(|| {
                            request
                                .get("widgetLines")
                                .and_then(Value::as_array)
                                .map(|lines| {
                                    ExtensionWidget::Lines(
                                        lines
                                            .iter()
                                            .filter_map(Value::as_str)
                                            .map(String::from)
                                            .collect(),
                                    )
                                })
                        });
                    self.set_extension_widget(&key, content, &placement);
                }
                // `setExtensionFooter`/`setExtensionHeader`
                // (`interactive-mode.ts:2664-2730`): a handle mounts the
                // adopted component, `undefined` restores the built-in.
                "setFooter" => self.set_extension_footer(
                    request
                        .get("handle")
                        .and_then(Value::as_u64)
                        .map(|value| value as u32),
                ),
                "setHeader" => self.set_extension_header(
                    request
                        .get("handle")
                        .and_then(Value::as_u64)
                        .map(|value| value as u32),
                ),
                // The working-status family (`interactive-mode.ts
                // :2501-2533`, `:2781-2789`).
                "setWorkingMessage" => self.set_working_message(
                    request
                        .get("message")
                        .and_then(Value::as_str)
                        .map(String::from),
                ),
                "setWorkingVisible" => self.set_working_visible(
                    request
                        .get("visible")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                ),
                "setWorkingIndicator" => self.set_working_indicator(
                    request
                        .get("indicator")
                        .and_then(Self::loader_indicator_from_json),
                ),
                "setHiddenThinkingLabel" => self.set_hidden_thinking_label(
                    request
                        .get("label")
                        .and_then(Value::as_str)
                        .map(String::from),
                ),
                // The editor text ops (`:2795-2797`).
                "pasteToEditor" => {
                    let text = request
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.paste_to_editor(&text);
                }
                "set_editor_text" => {
                    let text = request
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.set_editor_text(&text);
                }
                // `setToolsExpanded` (`:2823` → `:4745-4761`).
                "setToolsExpanded" => self.set_tools_expanded(
                    request
                        .get("expanded")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ),
                // The terminal-input listener count sync (`:2732-2741`).
                "terminalInputListeners" => {
                    self.terminal_input_listeners =
                        request.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
                }
                // `addAutocompleteProvider` (`:2799-2802`): the wrapper
                // joins the chain and the composed provider re-mounts.
                "addAutocompleteProvider" => {
                    let index = request.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let trigger_characters: Vec<char> = request
                        .get("triggerCharacters")
                        .and_then(Value::as_array)
                        .map(|characters| {
                            characters
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .filter_map(|character| character.chars().next())
                                .collect()
                        })
                        .unwrap_or_default();
                    self.autocomplete_wrappers.push(AutocompleteWrapperRecord {
                        index,
                        trigger_characters,
                    });
                    self.rebuild_autocomplete_provider();
                }
                // ---- E3 slice 9: the extension editor component ----
                // `setCustomEditorComponent` (`interactive-mode.ts:2979
                // -3055`): the glue already invoked the factory and wired
                // the component; the handle mounts the mode-side delegate.
                // Upstream swaps `this.editor` immediately and remounts the
                // container when the last dialog closes — the port's host
                // keeps its slot, so the delegate is visible on the next
                // render either way.
                "setEditorComponent" => {
                    let restore = request
                        .get("restore")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if restore {
                        self.restore_default_editor_component();
                    } else if let Some(handle) = request.get("handle").and_then(Value::as_u64) {
                        self.mount_extension_editor_component(handle as u32);
                    }
                }
                // The wired `onSubmit` (`interactive-mode.ts:2997` — the
                // component calls the default editor's callback): the
                // editor-events queue routes it through the same submit
                // pipeline next pump frame. Upstream's built-in editor
                // clears its own text before the callback; a custom
                // component owns its text lifecycle, so the drain skips
                // the clear while the delegate is live.
                "editorSubmit" => {
                    let text = request
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.editor_events
                        .lock()
                        .expect("editor events")
                        .push(EditorEventOut::Submit(text));
                }
                // The wired `onChange` (`interactive-mode.ts:3248-3253`):
                // the bash-mode flag tracks the component's text.
                "editorChange" => {
                    let text = request
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let is_bash_mode = text.trim_start().starts_with('!');
                    if let Some(editor) = self.editor_host_mut() {
                        if editor.is_bash_mode != is_bash_mode {
                            editor.is_bash_mode = is_bash_mode;
                            drop(editor);
                            self.update_editor_border_color();
                        }
                    }
                }
                // The CustomEditor-derived fallbacks + the actionHandlers
                // copy (`interactive-mode.ts:3024-3047`): the mode routes
                // them like the built-in editor's own handlers. `escape` →
                // the interrupt action (the port's escape binding), `ctrlD`
                // → exit; `pasteImage` is a no-op (the port has no
                // clipboard-image paste); `app:*` resolve through the
                // action table.
                "editorAction" => {
                    let action = request
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    match action.as_str() {
                        "escape" => self.handle_action(&AppAction::Interrupt),
                        "ctrlD" => self.handle_action(&AppAction::Exit),
                        "pasteImage" => {}
                        "extensionShortcut" => {
                            let data = request
                                .get("data")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            self.run_extension_shortcut(&data);
                        }
                        app => {
                            if let Some(resolved) =
                                Self::app_action_from_id(app.strip_prefix("app:").unwrap_or(app))
                            {
                                self.handle_action(&resolved);
                            }
                        }
                    }
                }
                // The composed-provider autocomplete read (the wire's
                // adapter, `interactive-mode.ts:3014-3016`): the pump
                // answers from the live composed chain.
                "editorAutocomplete" => {
                    let id = request
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let response = self.extension_editor_autocomplete(&request);
                    if let Some(bridge) = self.extension_ui_bridge.as_ref() {
                        bridge.respond(&id, &response);
                    }
                }
                // The factory adapter's `TUI.showOverlay` surface
                // (`tui.ts:518/:792-816`).
                // Deviation: the port tracks these mounts in the same
                // registry as the custom overlays so a reload disposes
                // their runner handles; upstream leaves the factory
                // surface's bookkeeping to the extension's own handle.
                "showOverlay" => self.mount_extension_overlay(&request, "options", None),
                // `TUI.hideOverlay()`'s topmost pop (`tui.ts:891-907`).
                "hideTopOverlay" => self.tui.hide_topmost_overlay(),
                // The handle's writes (`tui.ts:810-851`): `hide()` removes
                // this overlay from the TUI — the registry entry stays, so
                // a later done()/reset still runs `removeOverlay`.
                "overlayHide" => {
                    let id = request
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if let Some(overlay) = self.extension_overlay_id(&id) {
                        self.tui.hide_overlay(overlay);
                    }
                }
                "overlaySetHidden" => {
                    let id = request
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let hidden = request
                        .get("hidden")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if let Some(overlay) = self.extension_overlay_id(&id) {
                        self.tui.set_overlay_hidden(overlay, hidden);
                    }
                }
                "overlayFocus" => {
                    let id = request
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if let Some(overlay) = self.extension_overlay_id(&id) {
                        self.tui.focus_overlay(overlay);
                    }
                }
                // The handle's `unfocus(options?)` (`tui.ts:852-879`).
                // Deviation: an explicit target crosses as "focus nothing" —
                // component targets have no crossing representation.
                "overlayUnfocus" => {
                    let id = request
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let explicit = request
                        .get("explicit")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if let Some(overlay) = self.extension_overlay_id(&id) {
                        self.tui.overlay_unfocus(overlay, explicit.then_some(None));
                    }
                }
                // The handle's reads (`tui.ts:845/:883/:884-887`): the mode
                // answers from its registry, the delivery thread resolves
                // the glue deferred.
                "overlayIsHidden" | "overlayIsFocused" | "overlayGetBounds" => {
                    let id = request
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let overlay = self.extension_overlay_id(&id);
                    let response = match method.as_str() {
                        "overlayIsFocused" => {
                            serde_json::json!({ "focused": overlay.is_some_and(|o| self.tui.overlay_is_focused(o)) })
                        }
                        "overlayGetBounds" => {
                            match overlay.and_then(|o| self.tui.overlay_bounds(o)) {
                                Some(bounds) => serde_json::json!({
                                    "bounds": {
                                        "row": bounds.row,
                                        "col": bounds.col,
                                        "width": bounds.width,
                                        "height": bounds.height,
                                    }
                                }),
                                None => serde_json::json!({}),
                            }
                        }
                        _ => serde_json::json!({
                            "hidden": overlay.is_some_and(|o| self.tui.overlay_is_hidden(o).unwrap_or(false))
                        }),
                    };
                    if let Some(bridge) = self.extension_ui_bridge.as_ref() {
                        bridge.respond(&id, &response);
                    }
                }
                // `ctx.ui.setTitle` (`interactive-mode.ts:2793` →
                // `terminal.setTitle`, `terminal.ts:600-604`).
                "setTitle" => {
                    let title = request
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.tui.set_title(title);
                }
                // `showExtensionNotify` (`interactive-mode.ts:3061-3071`).
                "notify" => {
                    let message = request
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match request.get("notifyType").and_then(Value::as_str) {
                        Some("error") => self.show_error(message),
                        Some("warning") => self.show_warning(message),
                        _ => self.show_status(message),
                    }
                }
                // The custom factory's `tui.requestRender()` adapter.
                "requestRender" => self.tui.request_render(),
                // `setStatus` → `setExtensionStatus` (`interactive-mode.ts:2440-2443`
                // → `footer-data-provider.ts:146-152`): the glue omits
                // `statusText` when the extension passed undefined, which
                // deletes the key.
                "setStatus" => {
                    let key = request
                        .get("statusKey")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let text = request.get("statusText").and_then(Value::as_str);
                    self.set_extension_status(&key, text);
                }
                _ => {}
            }
        }
        self.pump_extension_ui_dialogs();
    }

    /// Builds one `ctx.ui` dialog request's stack entry without mounting
    /// (`showExtensionSelector`/`showExtensionConfirm`/
    /// `showExtensionInput`/`showExtensionEditor` —
    /// `interactive-mode.ts:2858-2955` — and the inline arm of
    /// `showExtensionCustom`, `:3074-3163`). A custom request without a
    /// runner or handle drops.
    #[allow(clippy::too_many_lines)] // mirrors the TS show*Extension* mounts; split when extending
    #[cfg(feature = "ext-js")]
    fn extension_dialog_entry(
        &mut self,
        request: &serde_json::Value,
    ) -> Option<ExtensionUiDialogEntry> {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let id = request
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let timeout_ms = request.get("timeout").and_then(Value::as_u64);
        let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        match method {
            "select" => {
                let title = request
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Select an option");
                let options: Vec<String> = request
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(|option| option.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let events = Arc::new(Mutex::new(Vec::new()));
                let selector = ExtensionSelectorComponent::new(title, options, Arc::clone(&events));
                Some(ExtensionUiDialogEntry {
                    id,
                    kind: ExtensionUiKind::Select,
                    events: ExtensionUiDialogEvents::Selector(events),
                    deadline,
                    last_seconds: None,
                    handle: None,
                    component: Some(Box::new(selector)),
                    // `showExtensionSelector`'s default blocked
                    // (`interactive-mode.ts:2864`).
                    blocked: Some(BlockedStatus {
                        kind: pi_tui::program_status::ProgramStatusKind::Question,
                        message: title.to_string(),
                    }),
                })
            }
            "confirm" => {
                // `${title}\n${message}` over Yes/No
                // (`showExtensionConfirm`, `interactive-mode.ts:2940-2955`).
                let title = request
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Confirm");
                let message = request.get("message").and_then(Value::as_str).unwrap_or("");
                let events = Arc::new(Mutex::new(Vec::new()));
                let selector = ExtensionSelectorComponent::new(
                    &format!("{title}\n{message}"),
                    vec!["Yes".to_string(), "No".to_string()],
                    Arc::clone(&events),
                );
                Some(ExtensionUiDialogEntry {
                    id,
                    kind: ExtensionUiKind::Confirm,
                    events: ExtensionUiDialogEvents::Selector(events),
                    deadline,
                    last_seconds: None,
                    handle: None,
                    component: Some(Box::new(selector)),
                    // `showExtensionConfirm`'s blocked
                    // (`interactive-mode.ts:2900-2903`).
                    blocked: Some(BlockedStatus {
                        kind: pi_tui::program_status::ProgramStatusKind::Permission,
                        message: title.to_string(),
                    }),
                })
            }
            "input" => {
                let title = request
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Enter a value");
                let placeholder = request.get("placeholder").and_then(Value::as_str);
                let events = Arc::new(Mutex::new(Vec::new()));
                let prompt = ExtensionInputComponent::new(title, placeholder, Arc::clone(&events));
                Some(ExtensionUiDialogEntry {
                    id,
                    kind: ExtensionUiKind::Input,
                    events: ExtensionUiDialogEvents::Input(events),
                    deadline,
                    last_seconds: None,
                    handle: None,
                    component: Some(Box::new(prompt)),
                    // `showExtensionInput`'s blocked
                    // (`interactive-mode.ts:2949`).
                    blocked: Some(BlockedStatus {
                        kind: pi_tui::program_status::ProgramStatusKind::Question,
                        message: title.to_string(),
                    }),
                })
            }
            "editor" => {
                let title = request
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Edit text");
                let prefill = request.get("prefill").and_then(Value::as_str);
                let events = Arc::new(Mutex::new(Vec::new()));
                let editor = ExtensionEditorComponent::new(title, prefill, Arc::clone(&events));
                Some(ExtensionUiDialogEntry {
                    id,
                    kind: ExtensionUiKind::Editor,
                    events: ExtensionUiDialogEvents::Editor(events),
                    deadline,
                    last_seconds: None,
                    handle: None,
                    component: Some(Box::new(editor)),
                    // `showExtensionEditor`'s blocked
                    // (`interactive-mode.ts:2983`).
                    blocked: Some(BlockedStatus {
                        kind: pi_tui::program_status::ProgramStatusKind::Question,
                        message: title.to_string(),
                    }),
                })
            }
            "custom" => {
                // The inline arm of `showExtensionCustom`: the wrapper
                // crosses render/input through the runner; done() closed
                // the promise glue-side, the customDone crossing closes
                // this entry. No timeout option upstream (`:3079-3086`).
                let handle = request.get("handle").and_then(Value::as_u64)? as u32;
                let runner = Arc::clone(self.extension_runner.as_ref()?);
                let component = ExtensionCustomComponent::new(runner, handle);
                Some(ExtensionUiDialogEntry {
                    id,
                    kind: ExtensionUiKind::Custom,
                    events: ExtensionUiDialogEvents::Custom,
                    deadline: None,
                    last_seconds: None,
                    handle: Some(handle),
                    component: Some(Box::new(component)),
                    // The inline custom's `options?.blocked`
                    // (`interactive-mode.ts:3159`); the glue crosses it
                    // with the overlay slice.
                    blocked: None,
                })
            }
            _ => None,
        }
    }

    /// The crossed `OverlayOptions` JSON → the port's options
    /// (`tui.ts:267-303`): sizes are numbers (cells) or `"N%"` strings
    /// (percent of the terminal dimension), the anchor names
    /// [`pi_tui::tui::OverlayAnchor`]'s arms, the margin is one number or a
    /// `{top,right,bottom,left}` object. Deviation: the `visible` predicate
    /// cannot cross JSON and never arrives; unknown fields drop.
    #[cfg(feature = "ext-js")]
    fn overlay_options_from_json(options: &Value) -> pi_tui::tui::OverlayOptions {
        use pi_tui::tui::{OverlayAnchor, OverlayMargin, OverlayOptions, SizeValue};
        let size_value = |value: &Value| -> Option<SizeValue> {
            if let Some(text) = value.as_str() {
                let percent = text.strip_suffix('%')?;
                return percent.trim().parse::<f64>().ok().map(SizeValue::Percent);
            }
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|f| f as i64))
                .map(SizeValue::Cells)
        };
        let offset = |value: Option<&Value>| value.and_then(Value::as_i64).unwrap_or(0);
        let mut parsed = OverlayOptions {
            width: options
                .get("width")
                .and_then(|v| (!v.is_null()).then_some(v))
                .and_then(size_value),
            min_width: options.get("minWidth").and_then(Value::as_i64),
            max_height: options
                .get("maxHeight")
                .and_then(|v| (!v.is_null()).then_some(v))
                .and_then(size_value),
            offset_x: offset(options.get("offsetX")),
            offset_y: offset(options.get("offsetY")),
            row: options
                .get("row")
                .and_then(|v| (!v.is_null()).then_some(v))
                .and_then(size_value),
            col: options
                .get("col")
                .and_then(|v| (!v.is_null()).then_some(v))
                .and_then(size_value),
            non_capturing: options
                .get("nonCapturing")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ..OverlayOptions::default()
        };
        parsed.anchor = match options.get("anchor").and_then(Value::as_str) {
            Some("top-left") => OverlayAnchor::TopLeft,
            Some("top-right") => OverlayAnchor::TopRight,
            Some("bottom-left") => OverlayAnchor::BottomLeft,
            Some("bottom-right") => OverlayAnchor::BottomRight,
            Some("top-center") => OverlayAnchor::TopCenter,
            Some("bottom-center") => OverlayAnchor::BottomCenter,
            Some("left-center") => OverlayAnchor::LeftCenter,
            Some("right-center") => OverlayAnchor::RightCenter,
            // `"center"` and unknown strings: the default anchor.
            _ => OverlayAnchor::Center,
        };
        if let Some(margin) = options.get("margin").filter(|value| !value.is_null()) {
            let mut margins = OverlayMargin::default();
            if let Some(all) = margin.as_i64() {
                margins.top = Some(all);
                margins.right = Some(all);
                margins.bottom = Some(all);
                margins.left = Some(all);
            } else if let Some(object) = margin.as_object() {
                margins.top = object.get("top").and_then(Value::as_i64);
                margins.right = object.get("right").and_then(Value::as_i64);
                margins.bottom = object.get("bottom").and_then(Value::as_i64);
                margins.left = object.get("left").and_then(Value::as_i64);
            }
            parsed.margin = margins;
        }
        parsed
    }

    /// The crossed `blocked` JSON → [`BlockedStatus`]
    /// (`showExtensionCustom`'s `options?.blocked`,
    /// `interactive-mode.ts:3085`).
    #[cfg(feature = "ext-js")]
    fn blocked_status_from_json(value: &Value) -> Option<BlockedStatus> {
        let kind = match value.get("kind").and_then(Value::as_str) {
            Some("permission") => pi_tui::program_status::ProgramStatusKind::Permission,
            Some("question") => pi_tui::program_status::ProgramStatusKind::Question,
            Some("auth") => pi_tui::program_status::ProgramStatusKind::Auth,
            _ => return None,
        };
        let message = value.get("message").and_then(Value::as_str)?.to_string();
        Some(BlockedStatus { kind, message })
    }

    /// The overlay registry's lookup by crossing id.
    #[cfg(feature = "ext-js")]
    fn extension_overlay_id(&self, id: &str) -> Option<pi_tui::tui::OverlayId> {
        self.extension_overlays
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.overlay)
    }

    /// The overlay mount (`showExtensionCustom`'s overlay arm,
    /// `interactive-mode.ts:3139-3157`, and the factory `TUI.showOverlay`
    /// surface, `tui.ts:792-816`): the adopted wrapper mounts through
    /// `show_overlay` with the crossed options (read from `options_field`)
    /// and joins the registry; a blocked custom takes a numbered
    /// `extension-overlay:N` source and reports it for the overlay's
    /// lifetime.
    #[cfg(feature = "ext-js")]
    fn mount_extension_overlay(
        &mut self,
        request: &Value,
        options_field: &str,
        blocked: Option<BlockedStatus>,
    ) {
        let Some(handle) = request
            .get("handle")
            .and_then(Value::as_u64)
            .map(|h| h as u32)
        else {
            return;
        };
        let Some(runner) = self.extension_runner.as_ref() else {
            return;
        };
        let id = request
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let component = ExtensionCustomComponent::new(Arc::clone(runner), handle);
        let options = request
            .get(options_field)
            .filter(|value| !value.is_null())
            .map(Self::overlay_options_from_json);
        // `overlayHandle = this.ui.showOverlay(c, resolveOptions())`
        // (`interactive-mode.ts:3153`).
        let overlay = self.tui.show_overlay(Box::new(component), options);
        // `overlayStatusSource` (`interactive-mode.ts:3094-3096`): numbered
        // per blocked overlay, so closing one keeps the others.
        let blocked_source = blocked.as_ref().map(|_| {
            let number = self.next_blocking_overlay_id;
            self.next_blocking_overlay_id += 1;
            format!("extension-overlay:{number}")
        });
        self.extension_overlays.push(ExtensionOverlayEntry {
            id,
            overlay,
            component_handle: Some(handle),
            blocked_source: blocked_source.clone(),
        });
        if let (Some(source), Some(blocked)) = (blocked_source, blocked) {
            let session_name = self.session.get_session_name();
            self.program_status.set_blocked(
                &source,
                Some(blocked),
                session_name.as_deref(),
                &mut self.tui,
            );
        }
    }

    /// `removeOverlay` (`interactive-mode.ts:3113-3120`): deleting the
    /// registry entry FIRST makes a second call a no-op; the overlay itself
    /// hides (another may have opened above it), the blocked source clears,
    /// and the component disposes.
    #[cfg(feature = "ext-js")]
    fn remove_extension_overlay(&mut self, id: &str) {
        let Some(index) = self
            .extension_overlays
            .iter()
            .position(|entry| entry.id == id)
        else {
            return;
        };
        let entry = self.extension_overlays.remove(index);
        self.tui.hide_overlay(entry.overlay);
        if let Some(source) = &entry.blocked_source {
            let session_name = self.session.get_session_name();
            self.program_status
                .set_blocked(source, None, session_name.as_deref(), &mut self.tui);
        }
        if let Some(handle) = entry.component_handle
            && let Some(runner) = self.extension_runner.as_ref()
        {
            let _ = runner
                .lock()
                .expect("extension runner mutex poisoned")
                .component_dispose(handle);
        }
    }

    /// `pushExtensionDialog` (`interactive-mode.ts:2833-2838`): a built-in
    /// selector showing is disposed, the dialog pushes, and the slot
    /// shows it. The displaced occupant parks by kind — the editor into
    /// `suspended_editor`, the previous top dialog into its entry
    /// ([`Self::park_displaced_occupant`]).
    #[cfg(feature = "ext-js")]
    fn push_extension_dialog_entry(&mut self, mut entry: ExtensionUiDialogEntry) {
        let component = entry
            .component
            .take()
            .expect("a fresh dialog entry carries its component");
        let focused = component_id(component.as_ref());
        let displaced = self
            .replace_editor_slot(component)
            .expect("the editor slot holds a component");
        self.park_displaced_occupant(displaced);
        self.extension_ui_dialogs.push(entry);
        self.tui.set_focus(Some(FocusTarget::Id(focused)));
        self.report_extension_dialog_blocked();
        self.tui.request_render();
    }

    /// `showTopExtensionDialog`'s program-status arm
    /// (`interactive-mode.ts:2847-2855`): the top dialog's `blocked` is the
    /// reported `extension-dialog` source while it shows; when the editor
    /// returns the source clears.
    #[cfg(feature = "ext-js")]
    fn report_extension_dialog_blocked(&mut self) {
        let blocked = self
            .extension_ui_dialogs
            .last()
            .and_then(|dialog| dialog.blocked.clone());
        let session_name = self.session.get_session_name();
        self.program_status.set_blocked(
            "extension-dialog",
            blocked,
            session_name.as_deref(),
            &mut self.tui,
        );
    }

    /// `setExtensionStatus` (`interactive-mode.ts:2440-2443`) over
    /// `FooterDataProvider.setExtensionStatus` (`footer-data-provider.ts:146-152`):
    /// an undefined text deletes the key; every change re-renders.
    #[cfg(feature = "ext-js")]
    fn set_extension_status(&mut self, key: &str, text: Option<&str>) {
        if let Some(footer) = self.footer_mut() {
            match text {
                Some(text) => {
                    footer
                        .data
                        .extension_statuses
                        .insert(key.to_string(), text.to_string());
                }
                None => {
                    footer.data.extension_statuses.remove(key);
                }
            }
        }
        self.tui.request_render();
    }

    /// The dialog stack's drain: per-dialog countdown ticks (the timers
    /// run inside the components upstream — hidden dialogs keep ticking,
    /// `extension-selector.ts:63-72`), then the event mailboxes. Only the
    /// visible top receives input, but a countdown expiry can close any
    /// dialog. The response shapes are the runner's (`{value}`,
    /// `{confirmed}`, `{cancelled:true}` — the glue parses them into the
    /// method's result).
    #[allow(clippy::too_many_lines)] // mirrors the TS dialog pump; split when extending
    #[cfg(feature = "ext-js")]
    fn pump_extension_ui_dialogs(&mut self) {
        if self.extension_ui_dialogs.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut ticked: Vec<(usize, u32)> = Vec::new();
        let mut expired: Vec<String> = Vec::new();
        for (index, dialog) in self.extension_ui_dialogs.iter().enumerate() {
            let Some(deadline) = dialog.deadline else {
                continue;
            };
            if now >= deadline {
                expired.push(dialog.id.clone());
            } else {
                let seconds = (deadline.saturating_duration_since(now).as_secs_f64().ceil()) as u32;
                if dialog.last_seconds != Some(seconds) {
                    ticked.push((index, seconds));
                }
            }
        }
        for (index, seconds) in ticked {
            if let Some(dialog) = self.extension_ui_dialogs.get_mut(index) {
                dialog.last_seconds = Some(seconds);
            }
            self.set_extension_ui_countdown(index, seconds);
        }
        for id in expired {
            self.close_extension_ui_dialog_by_id(&id, &serde_json::json!({ "cancelled": true }));
        }
        // The event mailboxes, per dialog.
        let mut drained: Vec<(String, ExtensionUiDialogEvent)> = Vec::new();
        for dialog in &self.extension_ui_dialogs {
            let events = match &dialog.events {
                ExtensionUiDialogEvents::Selector(events) => {
                    let taken: Vec<ExtensionSelectorEvent> =
                        std::mem::take(&mut *events.lock().expect("extension ui selector"));
                    taken
                        .into_iter()
                        .map(|event| match event {
                            ExtensionSelectorEvent::Selected(option) => {
                                ExtensionUiDialogEvent::Selected(option)
                            }
                            ExtensionSelectorEvent::Cancel => ExtensionUiDialogEvent::Cancelled,
                        })
                        .collect::<Vec<ExtensionUiDialogEvent>>()
                }
                ExtensionUiDialogEvents::Input(events) => {
                    let taken: Vec<ExtensionInputEvent> =
                        std::mem::take(&mut *events.lock().expect("extension ui input"));
                    taken
                        .into_iter()
                        .map(|event| match event {
                            ExtensionInputEvent::Submitted(value) => {
                                ExtensionUiDialogEvent::Submitted(value)
                            }
                            ExtensionInputEvent::Cancel => ExtensionUiDialogEvent::Cancelled,
                        })
                        .collect()
                }
                ExtensionUiDialogEvents::Editor(events) => {
                    let taken: Vec<ExtensionEditorEvent> =
                        std::mem::take(&mut *events.lock().expect("extension ui editor"));
                    taken
                        .into_iter()
                        .map(|event| match event {
                            ExtensionEditorEvent::Submitted(value) => {
                                ExtensionUiDialogEvent::Submitted(value)
                            }
                            ExtensionEditorEvent::Cancel => ExtensionUiDialogEvent::Cancelled,
                        })
                        .collect()
                }
                // A custom component interacts through the runner
                // crossings; the close crosses as customDone.
                ExtensionUiDialogEvents::Custom => continue,
            };
            for event in events {
                drained.push((dialog.id.clone(), event));
            }
        }
        for (id, event) in drained {
            let kind = self
                .extension_ui_dialogs
                .iter()
                .find(|dialog| dialog.id == id)
                .map(|dialog| dialog.kind);
            let response = match (event, kind) {
                (ExtensionUiDialogEvent::Submitted(value), Some(ExtensionUiKind::Confirm)) => {
                    serde_json::json!({ "confirmed": value == "Yes" })
                }
                (ExtensionUiDialogEvent::Selected(option), Some(ExtensionUiKind::Confirm)) => {
                    serde_json::json!({ "confirmed": option == "Yes" })
                }
                (ExtensionUiDialogEvent::Submitted(value), _) => {
                    serde_json::json!({ "value": value })
                }
                (ExtensionUiDialogEvent::Selected(option), _) => {
                    serde_json::json!({ "value": option })
                }
                (ExtensionUiDialogEvent::Cancelled, _) => {
                    serde_json::json!({ "cancelled": true })
                }
            };
            self.close_extension_ui_dialog_by_id(&id, &response);
        }
    }

    /// The countdown re-render for the dialog at `index`: the visible one
    /// (its entry holds no component) through the editor slot, a parked
    /// one through its entry (upstream's timer updates the component
    /// wherever it lives; the title suffix shows when it surfaces again).
    #[cfg(feature = "ext-js")]
    fn set_extension_ui_countdown(&mut self, index: usize, seconds: u32) {
        let visible = self
            .extension_ui_dialogs
            .get(index)
            .is_some_and(|dialog| dialog.component.is_none());
        if visible {
            if let Some(selector) =
                self.downcast_tree::<ExtensionSelectorComponent>(HostSlot::Editor)
            {
                selector.set_countdown_seconds(Some(seconds));
            } else if let Some(prompt) =
                self.downcast_tree::<ExtensionInputComponent>(HostSlot::Editor)
            {
                prompt.set_countdown_seconds(Some(seconds));
            }
        } else if let Some(component) = self
            .extension_ui_dialogs
            .get_mut(index)
            .and_then(|dialog| dialog.component.as_mut())
            && let Some(any) = component.as_any_mut()
        {
            if let Some(selector) = any.downcast_mut::<ExtensionSelectorComponent>() {
                selector.set_countdown_seconds(Some(seconds));
            } else if let Some(prompt) = any.downcast_mut::<ExtensionInputComponent>() {
                prompt.set_countdown_seconds(Some(seconds));
            }
        }
        self.tui.request_render();
    }

    /// The dialog's close path (`removeExtensionDialog` +
    /// `showTopExtensionDialog`, `interactive-mode.ts:2840-2856`): splice
    /// the dialog out by identity; if it was visible, the previous dialog
    /// (or the editor) shows again. The response answers the runner's
    /// wait for the mailbox kinds; a custom dialog's promise resolved
    /// glue-side at `done()` — the close only disposes its component.
    #[cfg(feature = "ext-js")]
    fn close_extension_ui_dialog_by_id(&mut self, id: &str, response: &serde_json::Value) {
        let Some(index) = self
            .extension_ui_dialogs
            .iter()
            .position(|dialog| dialog.id == id)
        else {
            return;
        };
        let was_top = index + 1 == self.extension_ui_dialogs.len();
        let entry = self.extension_ui_dialogs.remove(index);
        if let Some(bridge) = self.extension_ui_bridge.as_ref() {
            bridge.respond(id, response);
        }
        if was_top {
            // The slot holds the closed dialog: show the previous dialog
            // or the editor, focused like `showTopExtensionDialog`.
            if let Some(top) = self.extension_ui_dialogs.last_mut() {
                let component = top
                    .component
                    .take()
                    .expect("a parked top dialog carries its component");
                let focused = component_id(component.as_ref());
                drop(self.replace_editor_slot(component));
                self.tui.set_focus(Some(FocusTarget::Id(focused)));
            } else {
                let editor = self
                    .suspended_editor
                    .take()
                    .expect("the editor is parked while a dialog shows");
                drop(self.replace_editor_slot(editor));
                self.tui.set_focus(Some(FocusTarget::Id(self.editor_id)));
            }
            // `showTopExtensionDialog`'s blocked arm (`interactive-mode.ts:2853`):
            // the newly visible dialog's status, or none when the editor is
            // back.
            self.report_extension_dialog_blocked();
            self.tui.request_render();
        }
        drop(entry.component);
        // `disposeComponent` (`interactive-mode.ts:3104-3109`) for the
        // custom kinds; the mailbox components hold no engine state.
        if let Some(handle) = entry.handle
            && let Some(runner) = self.extension_runner.as_ref()
        {
            let _ = runner
                .lock()
                .expect("extension runner mutex poisoned")
                .component_dispose(handle);
        }
    }

    /// `resetExtensionUI`'s dialogs arm (`interactive-mode.ts:2592-2600`,
    /// called from init `:643` and `/reload` `:6628` — never on session
    /// switches): dispose every open dialog without calling `done()`, and
    /// remove the extension overlays newest-first — the same way `done()`
    /// does, leaving the promises pending as for inline dialogs
    /// (`interactive-mode.ts:2600-2602`). The port answers `cancelled` for
    /// the mailbox kinds so their delivery threads unblock (upstream leaves
    /// the promises pending — its engine is in-process and threadless). The
    /// footer's extension statuses clear unconditionally
    /// (`interactive-mode.ts:2607-2608`), like upstream's body runs whole.
    #[cfg(feature = "ext-js")]
    fn reset_extension_ui_dialogs(&mut self) {
        if !self.extension_ui_dialogs.is_empty() {
            for dialog in self.extension_ui_dialogs.drain(..) {
                if let Some(bridge) = self.extension_ui_bridge.as_ref() {
                    bridge.respond(&dialog.id, &serde_json::json!({ "cancelled": true }));
                }
                if let Some(handle) = dialog.handle
                    && let Some(runner) = self.extension_runner.as_ref()
                {
                    let _ = runner
                        .lock()
                        .expect("extension runner mutex poisoned")
                        .component_dispose(handle);
                }
                drop(dialog.component);
            }
            self.restore_editor();
            // `showTopExtensionDialog` with an empty stack
            // (`interactive-mode.ts:2598`): the `extension-dialog` blocked
            // source clears.
            self.report_extension_dialog_blocked();
        }
        // The overlays, newest first (`interactive-mode.ts:2602`): each
        // removal hides its own overlay, not the topmost, and leaves the
        // promise pending like the inline dialogs.
        let overlay_ids: Vec<String> = self
            .extension_overlays
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        for id in overlay_ids.into_iter().rev() {
            self.remove_extension_overlay(&id);
        }
        // `clearExtensionTerminalInputListeners` + the footer/header/
        // widget restores + the working-status resets (`interactive-mode
        // .ts:2603-2622`).
        #[cfg(feature = "ext-js")]
        {
            if let Some(runner) = self.extension_runner.as_ref() {
                let _ = runner
                    .lock()
                    .expect("extension runner mutex poisoned")
                    .clear_terminal_input_handlers();
            }
            self.terminal_input_listeners = 0;
            self.deferred_terminal_input.clear();
            self.set_extension_footer(None);
            self.set_extension_header(None);
            self.clear_extension_widgets();
            // `autocompleteProviderWrappers = []` +
            // `setupAutocompleteProvider()` (`:2609-2611`).
            if let Some(runner) = self.extension_runner.as_ref() {
                let _ = runner
                    .lock()
                    .expect("extension runner mutex poisoned")
                    .clear_autocomplete_wrappers();
            }
            self.autocomplete_wrappers.clear();
            self.rebuild_autocomplete_provider();
            // `setCustomEditorComponent(undefined)` (upstream's reset calls
            // it at `:2611` — the custom editor's text moves back, the
            // factory stash clears, and the status embedding reverts).
            self.restore_default_editor_component();
            if let Some(runner) = self.extension_runner.as_ref() {
                let _ = runner
                    .lock()
                    .expect("extension runner mutex poisoned")
                    .editor_component_clear_stash();
            }
            self.working_message = None;
            self.working_visible = true;
            self.set_working_indicator(None);
            // The active working indicator's message resets with the
            // interrupt hint (`:2617-2621`).
            let mut status = self.status_handle.lock().expect("status");
            if let Some(indicator) = status.as_mut()
                && indicator.kind == StatusKind::Working
            {
                indicator.set_message(format!(
                    "Working ({} to interrupt)",
                    theme::key_text(theme::app_keys::INTERRUPT)
                ));
            }
        }
        // `footerDataProvider.clearExtensionStatuses(); this.footer.invalidate();`
        // (`interactive-mode.ts:2607-2608`): upstream's `footer.invalidate`
        // is a no-op (`footer.ts:85-87`); the port's refresh recomputes the
        // session-derived cells instead.
        if let Some(footer) = self.footer_mut() {
            footer.data.extension_statuses.clear();
        }
        self.refresh_footer();
        // `updateTerminalTitle` (`interactive-mode.ts:2613`): the reload's
        // settings may carry a new session-independent title input.
        self.update_terminal_title();
        self.set_hidden_thinking_label(None);
    }

    /// `updateTerminalTitle` (`interactive-mode.ts:1173-1181`): the app
    /// title, the session name, and the cwd's basename, through
    /// `terminal.setTitle` (`terminal.ts:600-604`).
    fn update_terminal_title(&mut self) {
        let cwd = self.session.get_cwd().to_string_lossy().to_string();
        // `path.basename(cwd)`: an empty string at the filesystem root.
        let cwd_basename = std::path::Path::new(&cwd)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let title = match self.session.get_session_name() {
            // `APP_TITLE` (`config.ts:605`): the port has no `piConfigName`
            // build variant, so the default `π` applies.
            Some(name) => format!("π - {name} - {cwd_basename}"),
            None => format!("π - {cwd_basename}"),
        };
        self.tui.set_title(&title);
    }

    /// The settled credential (`completeProviderAuthentication`'s storage
    /// and status, `interactive-mode.ts:6277-6331`): store the oauth
    /// credential and report the saved path.
    #[cfg(feature = "ext-js")]
    fn finish_extension_login(&mut self, flow: &ExtensionLoginFlow, credential: serde_json::Value) {
        if let Some(runner) = self.extension_runner.as_ref() {
            let mut runner = runner.lock().expect("extension runner mutex poisoned");
            let _ = runner.oauth_login_end(flow.session);
        }
        self.extension_login_flow = None;
        self.restore_editor();
        let credential = match serde_json::from_value::<crate::auth::Credential>(credential) {
            Ok(credential) => credential,
            Err(error) => {
                self.show_error(&format!(
                    "Failed to save credentials for {}: {error}",
                    flow.provider_name
                ));
                return;
            }
        };
        let storage = crate::auth::AuthStorage::create(&self.auth_path);
        let result = storage.modify(&flow.provider_id, |_current| Ok(Some(credential)));
        match result {
            Ok(_) => self.show_status(&format!(
                "Logged in to {}. Credentials saved to {}",
                flow.provider_name,
                self.auth_path.display()
            )),
            Err(error) => self.show_error(&format!(
                "Failed to save credentials for {}: {error}",
                flow.provider_name
            )),
        }
    }

    /// A failed login (`showLoginDialog`'s catch, `:6584-6592`): report and
    /// drop the flow. Only `Login cancelled` routes to `onBack`; errors stay.
    #[cfg(feature = "ext-js")]
    fn finish_extension_login_error(&mut self, flow: &ExtensionLoginFlow, message: &str) {
        if let Some(runner) = self.extension_runner.as_ref() {
            let mut runner = runner.lock().expect("extension runner mutex poisoned");
            let _ = runner.oauth_login_end(flow.session);
        }
        self.extension_login_flow = None;
        self.extension_login_prompt = None;
        self.extension_login_select = None;
        self.restore_editor();
        self.show_error(&format!(
            "Failed to login to {}: {message}",
            flow.provider_name
        ));
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
                    // The built-in editor clears its own text before the
                    // callback; a custom component owns its text lifecycle
                    // (`onSubmit` upstream carries no clear — E3 slice 9).
                    #[cfg(feature = "ext-js")]
                    if self
                        .extension_editor
                        .lock()
                        .expect("extension editor")
                        .is_some()
                    {
                        self.submit(&text);
                        continue;
                    }
                    self.editor_mut().editor.set_text("");
                    self.submit(&text);
                }
                EditorEventOut::Action(action) => self.handle_action(&action),
                EditorEventOut::BashModeChanged => self.update_editor_border_color(),
                #[cfg(feature = "ext-js")]
                EditorEventOut::ExtensionShortcut(key) => self.run_extension_shortcut(&key),
            }
        }

        // Selector events, drained through the Arcs the components publish
        // to (`showSelector`'s onSelect/onCancel callbacks).
        self.pump_model_selector();
        self.pump_scoped_models_selector();
        self.pump_trust_selector();
        self.pump_settings_selector();
        self.pump_session_selector();
        self.pump_import_selector();
        self.pump_user_message_selector();
        self.pump_tree_selector();
        self.pump_tree_summary_dialog();
        self.pump_tree_prompt_editor();
        self.pump_branch_summary();
        self.pump_auth_selector();
        self.pump_login_prompt();
        #[cfg(feature = "ext-js")]
        {
            self.pump_extension_login_prompt();
            self.pump_extension_login_select();
            self.pump_extension_oauth_login();
            self.pump_extension_ui();
            // The deferred terminal-input chunks retry (the engine was
            // busy at dispatch time), then the ui-op snapshot refreshes
            // (E3 slice 7). The editor delegate's deferred keys/text
            // writes flush the same way (E3 slice 9).
            self.pump_deferred_terminal_input();
            if let Some(live) = self.extension_editor.lock().expect("extension editor").as_mut() {
                live.flush_pending();
            }
            self.refresh_ui_snapshot();
        }

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
            && let Some(handle) = self.easter_egg.take()
        {
            self.tui.hide_overlay(handle.overlay);
            self.tui.request_render();
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
                // The turn's prompts and context: a retry or overflow
                // recovery re-runs the same turn (`agent.continue()`).
                self.last_run_turn = Some((run.prompts, run.context));
            }
            self.set_working_status(None);
            // `_handlePostAgentRun` (`agent-session.ts:2062-2098`): the
            // retry decision first — compaction only when no retry was
            // scheduled.
            if self.last_agent_end_will_retry {
                self.start_retry_wait();
            } else {
                if self.retry_attempt > 0 {
                    // `auto_retry_end` on the final failure (:2081-2089);
                    // a successful retry just resets the budget (:1252).
                    let failed = self
                        .last_agent_end_messages
                        .as_ref()
                        .is_some_and(|messages| !crate::modes::retry::run_succeeded(messages));
                    if failed {
                        let error = self.last_agent_end_messages.as_deref().map_or_else(
                            || "Unknown error".to_string(),
                            crate::modes::retry::last_error_text,
                        );
                        self.show_error(&format!(
                            "Retry failed after {} attempts: {error}",
                            self.retry_attempt
                        ));
                    }
                    self.retry_attempt = 0;
                }
                self.post_run_compaction(false);
            }
            self.tui.request_render();
        }

        // The retry backoff fires (`_prepareRetry`'s sleep resolves; the
        // loop continues with the same turn) or is cancelled
        // (`session.abortRetry()` → `_finishCancelledRetry`).
        let mut retry_wait_outcome: Option<RetryWaitEvent> = None;
        if let Some(wait) = &self.retry_wait {
            loop {
                match wait.events.try_recv() {
                    Ok(outcome) => retry_wait_outcome = Some(outcome),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        retry_wait_outcome = Some(RetryWaitEvent::Cancelled);
                        break;
                    }
                }
            }
        }
        if let Some(outcome) = retry_wait_outcome {
            if let Some(wait) = self.retry_wait.take()
                && let Some(handle) = wait.handle
            {
                let _ = handle.join();
            }
            self.set_working_status(None);
            match outcome {
                RetryWaitEvent::Fire => self.spawn_retry_run(),
                RetryWaitEvent::Cancelled => {
                    let attempt = self.retry_attempt;
                    self.retry_attempt = 0;
                    self.show_error(&format!(
                        "Retry failed after {attempt} attempts: Retry cancelled"
                    ));
                }
            }
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
                // While retrying, interrupt cancels the backoff
                // (`session.abortRetry()`, `interactive-mode.ts:3874-3877`);
                // while compacting, it cancels the compaction
                // (`CompactionStatusIndicator`'s cancel hint + the escape
                // override at `interactive-mode.ts:3810-3813`); while a
                // branch summary runs, it cancels the summarizer (the
                // escape override at `:5829-5832`).
                let branch_summary_run = self
                    .tree_navigation
                    .as_ref()
                    .and_then(|navigation| navigation.summary_run.as_ref());
                if let Some(run) = &self.retry_wait {
                    run.abort.abort();
                } else if let Some(run) = branch_summary_run {
                    run.abort.abort();
                } else if let Some(run) = &self.compaction_run {
                    run.abort.abort();
                } else if let Some(run) = &self.agent_run {
                    run.abort.abort();
                }
            }
            AppAction::CycleModel(direction) => self.cycle_model(*direction),
            AppAction::CycleThinking => self.cycle_thinking_level(),
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
            // `toggleToolOutputExpansion` (`interactive-mode.ts:4741-4743`).
            AppAction::ToolsExpand => self.set_tools_expanded(!self.tool_output_expanded),
        }
    }

    /// `setToolsExpanded` (`interactive-mode.ts:4745-4761`): the flag, the
    /// ACTIVE header's `setExpanded` (a custom header duck-types through
    /// the bridge, `:4749-4752`), every expandable chat row, and the status
    /// line. The ctrl+o toggle and `ctx.ui.setToolsExpanded` both land
    /// here.
    fn set_tools_expanded(&mut self, expanded: bool) {
        if expanded == self.tool_output_expanded {
            return;
        }
        self.tool_output_expanded = expanded;
        #[cfg(feature = "ext-js")]
        let custom_header_active = self.custom_header.is_some();
        #[cfg(not(feature = "ext-js"))]
        let custom_header_active = false;
        if custom_header_active {
            #[cfg(feature = "ext-js")]
            if let Some(chrome) = self.custom_header.as_ref() {
                self.set_custom_header_expanded(chrome.handle, expanded);
            }
        } else if let Some(header) = self.header_mut() {
            header.set_expanded(expanded);
        }
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
            // `setToolOutputExpanded` duck-types every chat child
            // with a `setExpanded` (`interactive-mode.ts:4753-4758`)
            // — the custom rows both have one.
            #[cfg(feature = "ext-js")]
            if let Some(component) =
                self.downcast_chat::<super::components::custom_entry::CustomEntryComponent>(index)
            {
                component.set_expanded(expanded);
            }
            #[cfg(feature = "ext-js")]
            if let Some(component) = self
                .downcast_chat::<super::components::custom_message::CustomMessageComponent>(index)
            {
                component.set_expanded(expanded);
            }
        }
        self.show_status(&format!(
            "Tool output: {}",
            if expanded { "expanded" } else { "collapsed" }
        ));
        self.tui.request_render();
    }

    /// The custom header's `setExpanded` through the handle bridge — split
    /// out so the runner lock never nests under a tree borrow.
    #[cfg(feature = "ext-js")]
    fn set_custom_header_expanded(&self, handle: u32, expanded: bool) {
        if let Some(runner) = self.extension_runner.as_ref() {
            let _ = runner
                .lock()
                .expect("extension runner mutex poisoned")
                .component_set_expanded(handle, expanded);
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
    use base64::Engine as _;

    /// The template's session-data script tag (`template.html`), which the
    /// export tests decode the base64 payload out of.
    const SESSION_DATA_MARKER: &str = "<script id=\"session-data\" type=\"application/json\">";

    // --------------------------------------------------------------
    // /export and /import (`getPathCommandArgument` and the
    // `interactive-mode-import-command.test.ts` cases)
    // --------------------------------------------------------------

    /// A mode whose session is persisted in `dir`, so `/import` has a real
    /// session dir to copy into.
    fn persisted_fixture(dir: &std::path::Path) -> (InteractiveMode, FrameCapture) {
        let mut runtime = test_runtime(
            dir,
            Arc::new(ScriptedStream {
                calls: AtomicUsize::new(0),
                file_path: "unused".to_string(),
            }),
        );
        runtime.session =
            SessionManager::create(dir, Some(dir), &NewSessionOptions::default()).expect("session");
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 100,
            rows: 40,
        });
        let capture = terminal.capture();
        let options = InteractiveOptions {
            auth_path: Some(dir.join("auth.json")),
            ..InteractiveOptions::default()
        };
        let mut mode = InteractiveMode::new(&mut runtime, &options, Box::new(terminal));
        mode.tui.start().expect("tui start");
        (mode, capture)
    }

    /// A source session with one user message, persisted under `dir`.
    fn source_session(dir: &std::path::Path, text: &str) -> std::path::PathBuf {
        let mut session =
            SessionManager::create(dir, Some(dir), &NewSessionOptions::default()).expect("source");
        session
            .append_message(pi_agent::transcript::user_message(text, 1))
            .expect("append");
        session
            .get_session_file()
            .expect("source file")
            .to_path_buf()
    }

    /// `interactive-mode-import-command.test.ts:25-43`: quoted paths strip
    /// their quotes, apostrophes survive unquoted, and command token
    /// boundaries hold.
    #[test]
    fn path_command_argument_matches_upstream_parsing() {
        assert_eq!(
            InteractiveMode::get_path_command_argument(
                "/import \"path/to/session.jsonl\"",
                "/import"
            ),
            Some("path/to/session.jsonl")
        );
        assert_eq!(
            InteractiveMode::get_path_command_argument(
                "/import \"path with spaces/session.jsonl\"",
                "/import"
            ),
            Some("path with spaces/session.jsonl")
        );
        // Apostrophes survive in unquoted arguments.
        assert_eq!(
            InteractiveMode::get_path_command_argument("/import john's/session.jsonl", "/import"),
            Some("john's/session.jsonl")
        );
        // Command token boundaries: `/important` and `/exporter` do not parse.
        assert_eq!(
            InteractiveMode::get_path_command_argument("/important /tmp/session.jsonl", "/import"),
            None
        );
        assert_eq!(
            InteractiveMode::get_path_command_argument("/exporter out.html", "/export"),
            None
        );
        assert_eq!(
            InteractiveMode::get_path_command_argument("/import /tmp/session.jsonl", "/import"),
            Some("/tmp/session.jsonl")
        );
        // A bare command and a bare trailing space parse to None.
        assert_eq!(
            InteractiveMode::get_path_command_argument("/import", "/import"),
            None
        );
        assert_eq!(
            InteractiveMode::get_path_command_argument("/import  ", "/import"),
            None
        );
    }

    /// The picker advertises `/export` and `/import` with the verbatim
    /// upstream descriptions (`slash-commands.ts:25-26`); `/share`
    /// (`slash-commands.ts:27`) stays absent — the Radius gateway it uploads
    /// to is banned here.
    #[test]
    fn autocomplete_advertises_export_and_import_but_not_share() {
        let commands =
            InteractiveMode::autocomplete_commands(&[], Arc::new(Mutex::new(Vec::new())));
        let find = |name: &str| {
            commands
                .iter()
                .find(|command| command.name == name)
                .and_then(|command| command.description.as_deref())
        };
        assert_eq!(
            find("export"),
            Some("Export session (HTML default, or specify path: .html/.jsonl)")
        );
        assert_eq!(
            find("import"),
            Some("Import a JSONL session, preserving its ID and working directory")
        );
        assert_eq!(find("share"), None);
        // Neither command carries an argument hint upstream.
        for name in ["export", "import"] {
            let command = commands.iter().find(|c| c.name == name).expect(name);
            assert_eq!(command.argument_hint, None, "{name} carries no hint");
        }
    }

    #[test]
    fn import_without_a_path_shows_the_usage_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = persisted_fixture(dir.path());
        mode.submit("/import");
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Usage: /import <path.jsonl>"), "{plain}");
        assert!(mode.suspended_editor.is_none(), "no dialog for usage");
    }

    #[test]
    fn import_confirm_cancel_reports_and_restores() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = persisted_fixture(dir.path());
        mode.submit("/import nowhere.jsonl");
        mode.tui.render_now(false);
        assert!(mode.suspended_editor.is_some(), "confirm not mounted");
        let selector = mode
            .downcast_tree::<ExtensionSelectorComponent>(HostSlot::Editor)
            .expect("selector in the editor slot");
        let rendered = strip_terminal_sequences(&selector.render(100).join("\n"));
        assert!(rendered.contains("Import session"), "{rendered}");
        assert!(
            rendered.contains("Replace current session with nowhere.jsonl?"),
            "{rendered}"
        );
        assert!(rendered.contains("→ Yes"), "{rendered}");

        // Escape cancels: the confirm resolves false ("Import cancelled").
        mode.import_selector_events
            .as_ref()
            .expect("import events")
            .lock()
            .expect("events lock")
            .push(ExtensionSelectorEvent::Cancel);
        mode.pump();
        assert!(mode.suspended_editor.is_none(), "editor not restored");
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Import cancelled"), "{plain}");
    }

    #[test]
    fn import_confirm_yes_copies_and_swaps_the_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source_dir = tempfile::tempdir().expect("source dir");
        // The source rides a short name: the status line must fit the
        // 100-column capture without wrapping (the session-layout file name
        // wrapped it and the contains-assert missed it).
        let source_path = source_dir.path().join("source.jsonl");
        std::fs::copy(
            source_session(source_dir.path(), "imported hello"),
            &source_path,
        )
        .expect("copy source");
        let source = SessionManager::open(&source_path, None, None).expect("source open");
        let source_id = source.get_session_id().to_string();

        let (mut mode, capture) = persisted_fixture(dir.path());
        mode.submit(&format!("/import {}", source_path.display()));
        mode.import_selector_events
            .as_ref()
            .expect("import events")
            .lock()
            .expect("events lock")
            .push(ExtensionSelectorEvent::Selected("Yes".to_string()));
        mode.pump();
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        assert!(
            plain.contains(&format!("Session imported from: {}", source_path.display())),
            "{plain}"
        );
        // The session store swapped to the imported session (its id and its
        // entries replayed), and the file was copied into the mode's session
        // dir under the source's basename.
        assert_eq!(mode.session.get_session_id(), source_id);
        let imported_text = mode.session.get_entries().iter().any(|entry| {
            matches!(
                entry,
                SessionEntry::Message(entry)
                    if matches!(&entry.message, AgentMessage::User(user)
                        if matches!(&user.content,
                            pi_ai::types::UserContent::Text(text) if text == "imported hello"))
            )
        });
        assert!(imported_text, "imported entries did not replay");
        let destination = dir
            .path()
            .join(source_path.file_name().expect("source basename"));
        assert!(destination.exists(), "source copied into the session dir");
        assert!(mode.suspended_editor.is_none(), "editor restored");
    }

    #[test]
    fn import_missing_file_reports_the_upstream_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = persisted_fixture(dir.path());
        mode.submit("/import /nonexistent/nope.jsonl");
        mode.import_selector_events
            .as_ref()
            .expect("import events")
            .lock()
            .expect("events lock")
            .push(ExtensionSelectorEvent::Selected("Yes".to_string()));
        mode.pump();
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(
            plain.contains("Failed to import session: File not found:"),
            "{plain}"
        );
        // The non-fatal path keeps the session untouched.
        assert!(mode.suspended_editor.is_none(), "editor restored");
    }

    /// A header pointing at a dead cwd mounts the missing-cwd prompt; the
    /// continue option re-imports with the override, the fork option forks
    /// into the fallback cwd.
    #[test]
    fn import_missing_cwd_prompt_offers_continue_fork_cancel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source_dir = tempfile::tempdir().expect("source dir");
        let dead_cwd = source_dir.path().join("does-not-exist");
        let source_path = source_dir.path().join("stale.jsonl");
        std::fs::write(
            &source_path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"stale-session\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"{}\"}}\n",
                dead_cwd.display()
            ),
        )
        .expect("write stale session");
        let fallback_cwd = std::env::current_dir().expect("cwd");

        let (mut mode, capture) = persisted_fixture(dir.path());

        // Confirm → import → missing-cwd prompt.
        mode.submit(&format!("/import {}", source_path.display()));
        mode.import_selector_events
            .as_ref()
            .expect("import events")
            .lock()
            .expect("events lock")
            .push(ExtensionSelectorEvent::Selected("Yes".to_string()));
        mode.pump();
        mode.tui.render_now(false);
        assert!(mode.suspended_editor.is_some(), "cwd prompt not mounted");
        let plain = strip_terminal_sequences(&capture.output());
        assert!(
            plain.contains(&format!(
                "Session cwd does not exist: {}",
                dead_cwd.display()
            )),
            "{plain}"
        );

        // The prompt lists the upstream options; Continue re-imports with
        // the fallback cwd.
        let labels = crate::session::cwd::get_missing_session_cwd_options(
            &crate::session::cwd::SessionCwdIssue {
                session_file: None,
                session_cwd: dead_cwd.clone(),
                // The real issue's fallback is the runtime's process cwd
                // (`agent-session-runtime.ts:391`); the labels must match it
                // for the pump's label lookup to hit.
                fallback_cwd: fallback_cwd.clone(),
            },
        );
        mode.import_selector_events
            .as_ref()
            .expect("import events")
            .lock()
            .expect("events lock")
            .push(ExtensionSelectorEvent::Selected(labels[0].label.clone()));
        mode.pump();
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(
            plain.contains(&format!("Session imported from: {}", source_path.display())),
            "{plain}"
        );
        // The stored cwd was overridden by the fallback.
        assert_eq!(mode.session.get_cwd(), fallback_cwd);
        // The copied file was re-imported with the same id.
        assert_eq!(mode.session.get_session_id(), "stale-session");
    }

    /// The fork option of the missing-cwd prompt: a new session id in the
    /// fallback cwd, the original copy kept.
    #[test]
    fn import_missing_cwd_fork_creates_a_new_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source_dir = tempfile::tempdir().expect("source dir");
        let dead_cwd = source_dir.path().join("does-not-exist");
        let source_path = source_dir.path().join("stale.jsonl");
        std::fs::write(
            &source_path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"stale-session\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"{}\"}}\n",
                dead_cwd.display()
            ),
        )
        .expect("write stale session");
        let fallback_cwd = std::env::current_dir().expect("cwd");

        let (mut mode, capture) = persisted_fixture(dir.path());
        mode.submit(&format!("/import {}", source_path.display()));
        mode.import_selector_events
            .as_ref()
            .expect("import events")
            .lock()
            .expect("events lock")
            .push(ExtensionSelectorEvent::Selected("Yes".to_string()));
        mode.pump();

        let labels = crate::session::cwd::get_missing_session_cwd_options(
            &crate::session::cwd::SessionCwdIssue {
                session_file: None,
                session_cwd: dead_cwd.clone(),
                // The real issue's fallback is the runtime's process cwd
                // (`agent-session-runtime.ts:391`); the labels must match it
                // for the pump's label lookup to hit.
                fallback_cwd: fallback_cwd.clone(),
            },
        );
        mode.import_selector_events
            .as_ref()
            .expect("import events")
            .lock()
            .expect("events lock")
            .push(ExtensionSelectorEvent::Selected(labels[1].label.clone()));
        mode.pump();
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Forked into new session "), "{plain}");
        // A fresh id in the fallback cwd; the copy of the original stayed.
        assert_ne!(mode.session.get_session_id(), "stale-session");
        assert_eq!(mode.session.get_cwd(), fallback_cwd);
        assert!(
            mode.session
                .get_session_file()
                .expect("fork file")
                .parent()
                .expect("parent")
                == dir.path(),
            "fork rides in the current session dir"
        );
    }

    /// `/export <path>.jsonl` writes the branch export; `/export <path>.html`
    /// writes the viewer page. Both report the resolved path.
    #[test]
    fn export_writes_jsonl_and_html_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = persisted_fixture(dir.path());
        mode.session
            .append_message(pi_agent::transcript::user_message("exported hello", 1))
            .expect("append");

        let jsonl = dir.path().join("out.jsonl");
        mode.submit(&format!("/export {}", jsonl.display()));
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(
            plain.contains(&format!("Session exported to: {}", jsonl.display())),
            "{plain}"
        );
        let exported = std::fs::read_to_string(&jsonl).expect("exported jsonl");
        assert!(
            exported.starts_with("{\"type\":\"session\",\"version\":3,"),
            "{exported}"
        );

        let html = dir.path().join("out.html");
        mode.submit(&format!("/export {}", html.display()));
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(
            plain.contains(&format!("Session exported to: {}", html.display())),
            "{plain}"
        );
        let page = std::fs::read_to_string(&html).expect("exported html");
        // The payload rides base64-encoded in the session-data script tag.
        let payload_start = page
            .find(SESSION_DATA_MARKER)
            .map(|index| index + SESSION_DATA_MARKER.len())
            .expect("session data script tag");
        let payload = &page[payload_start..page.find("</script>").expect("script close")];
        let decoded = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .expect("base64"),
        )
        .expect("utf8");
        assert!(decoded.contains("exported hello"), "{decoded}");
    }

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

    /// `/settings` mounts the dialog without crashing (user-reported crash
    /// on the release binary; this drives the real submit path).
    #[test]
    fn settings_dialog_mounts_on_submit() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        mode.submit("/settings");
        assert!(
            mode.suspended_editor.is_some(),
            "the settings dialog occupies the editor slot"
        );
    }

    /// Toggling Auto-compact fires `AutoCompactChange` mid-dialog — with the
    /// editor parked in `suspended_editor` (Enter on the empty search box
    /// activates row 0; the user-reported panic). The border refresh reaches
    /// the parked host, and the toggle persists.
    #[test]
    fn settings_autocompact_change_reaches_the_parked_editor() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        mode.submit("/settings");
        assert!(mode.suspended_editor.is_some(), "editor parked");
        let before = mode.settings.get_compaction_enabled();
        mode.handle_settings_event(SettingsSelectorEvent::AutoCompactChange(!before));
        assert_eq!(
            mode.settings.get_compaction_enabled(),
            !before,
            "the toggle persisted"
        );
        let border = mode
            .suspended_editor
            .as_mut()
            .and_then(|host| host.as_any_mut())
            .and_then(|any| any.downcast_mut::<ChatEditor>())
            .expect("parked editor")
            .editor
            .border_color()
            .clone();
        assert_eq!(
            border("-"),
            theme::thinking_border_styler(mode.thinking_level)("-"),
            "the parked editor carries the refreshed border"
        );
    }

    /// A `/model` switch updates the footer's `(provider)` prefix together
    /// with the model id (user-reported: the boot provider survived).
    #[test]
    fn apply_model_updates_the_footer_provider_prefix() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        assert_eq!(mode.model.provider, "test");
        let other = pi_ai::types::Model {
            id: "b".to_string(),
            name: "B".to_string(),
            api: "test".to_string(),
            provider: "fixtureb".to_string(),
            reasoning: true,
            context_window: 200_000,
            max_tokens: 8192,
            ..pi_ai::types::Model::default()
        };
        mode.apply_model(other).expect("apply");
        let footer = mode
            .downcast_tree::<FooterComponent>(HostSlot::Footer)
            .expect("footer");
        assert_eq!(footer.data.model_id, "b");
        assert_eq!(footer.data.model_provider, "fixtureb");
    }

    /// Removes a runtime override when dropped (panic-safe cleanup of the
    /// process-global `RUNTIME_API_KEYS`).
    struct DropFlag(&'static str);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            crate::oauth::remove_runtime_api_key(self.0);
        }
    }

    /// The run credential follows the CURRENT provider: the `--api-key`
    /// runtime override applies to the provider it was registered for
    /// (`main.ts:898-906`) and a switched-to provider resolves its own
    /// stored credential (user-reported: requests kept the boot key).
    #[test]
    fn run_wire_credential_resolves_the_current_provider() {
        // Serializes the process-global `RUNTIME_API_KEYS` with the oauth
        // tests that mutate it.
        let _rebuild = crate::models_state::REBUILD_LOCK.lock().expect("lock");
        let _cleanup = DropFlag("groq");
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        std::fs::write(
            dir.path().join("auth.json"),
            serde_json::json!({
                "groq": {"type": "api_key", "key": "sk-groq-stored"},
                "anthropic": {"type": "api_key", "key": "sk-anthropic-stored"},
            })
            .to_string(),
        )
        .expect("auth.json");
        crate::oauth::set_runtime_api_key("groq", "sk-flag");
        let model = |provider: &str| pi_ai::types::Model {
            id: format!("{provider}-m"),
            name: provider.to_string(),
            api: "test".to_string(),
            provider: provider.to_string(),
            reasoning: true,
            context_window: 200_000,
            max_tokens: 8192,
            ..pi_ai::types::Model::default()
        };
        // The boot fixture provider is registered nowhere: no stored
        // credential, no override, no env mapping.
        assert_eq!(mode.run_wire_credential(), (None, None));
        mode.apply_model(model("groq")).expect("apply");
        let (api_key, access_token) = mode.run_wire_credential();
        assert_eq!(
            api_key.as_deref(),
            Some("sk-flag"),
            "override for its provider"
        );
        assert!(access_token.is_none());
        mode.apply_model(model("anthropic")).expect("apply");
        let (api_key, access_token) = mode.run_wire_credential();
        assert_eq!(
            api_key.as_deref(),
            Some("sk-anthropic-stored"),
            "the switched-to provider's stored credential"
        );
        assert!(access_token.is_none());
        crate::oauth::remove_runtime_api_key("groq");
        mode.apply_model(model("groq")).expect("apply");
        let (api_key, _) = mode.run_wire_credential();
        assert_eq!(
            api_key.as_deref(),
            Some("sk-groq-stored"),
            "the stored credential under the removed override"
        );
    }

    /// shift+tab cycles the thinking level over the model's supported list
    /// (`interactive-mode.ts:4711-4721`) and repaints the border with the
    /// new level's color.
    #[test]
    fn cycle_thinking_action_advances_the_level_and_border() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        assert!(mode.model.reasoning, "the fixture model reasons");
        let start = mode.thinking_level;
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Action(AppAction::CycleThinking));
        mode.pump();
        assert_ne!(mode.thinking_level, start, "the level advanced");
        let border = mode.editor_mut().editor.border_color().clone();
        assert_ne!(border("-"), "-", "the border carries a color token");
    }

    /// Typing `!` flips bash mode and repaints the border through the
    /// editor-change event (`interactive-mode.ts:3248-3253`); clearing the
    /// flag restores the thinking border (`:4700-4707`).
    #[test]
    fn bash_mode_border_tracks_the_draft() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        // The thinking border for a known level (`thinkingOff` is neutral,
        // `bashMode` green — never equal in the system theme).
        mode.thinking_level = ThinkingLevel::Off;
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::BashModeChanged);
        mode.pump();
        let thinking_border = mode.editor_mut().editor.border_color().clone();
        let editor = mode.editor_mut();
        editor.set_focused(true);
        let _ = pi_tui::component::Component::handle_input(editor, "!");
        mode.pump();
        let bash_border = mode.editor_mut().editor.border_color().clone();
        assert_ne!(
            bash_border("-"),
            thinking_border("-"),
            "bash mode recolors the border"
        );
        mode.editor_mut().is_bash_mode = false;
        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::BashModeChanged);
        mode.pump();
        let restored = mode.editor_mut().editor.border_color().clone();
        assert_eq!(
            restored("-"),
            thinking_border("-"),
            "clearing the flag restores the thinking border"
        );
    }

    /// The border stylers map the upstream theme tokens
    /// (`theme.ts:348-372`): thinking levels and bash mode carry distinct
    /// colors. (The fallback test theme renders `thinkingHigh` and
    /// `bashMode` alike, so only the deterministic pairs assert.)
    #[test]
    fn border_stylers_follow_the_thinking_tokens() {
        let off = theme::thinking_border_styler(ThinkingLevel::Off);
        let high = theme::thinking_border_styler(ThinkingLevel::High);
        let bash = theme::bash_mode_border_styler();
        assert_ne!(off("-"), high("-"), "levels map to distinct tokens");
        assert_ne!(off("-"), bash("-"), "bash mode is its own token");
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

    /// A mode whose extension runner has `source` loaded — the constructor's
    /// extension wiring (theme surface, keybindings, shortcuts) runs over it
    /// exactly like a real boot with an extensions directory.
    #[cfg(feature = "ext-js")]
    fn fixture_with_extension(
        dir: &std::path::Path,
        source: &str,
    ) -> (InteractiveMode, FrameCapture) {
        let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
            cwd: dir.to_string_lossy().to_string(),
            mode: "tui".to_string(),
            settings: serde_json::json!({}),
            ..Default::default()
        })
        .expect("runner");
        runner
            .load_extension("<test-0>", Some(source))
            .expect("load");
        runner.bind().expect("bind");
        let stream: Arc<dyn pi_agent::types::StreamFn> = Arc::new(ScriptedStream {
            calls: AtomicUsize::new(0),
            file_path: "unused".to_string(),
        });
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
        runtime.extension_runner = Some(Arc::new(std::sync::Mutex::new(runner)));
        let mode = InteractiveMode::new(&mut runtime, &options, Box::new(terminal));
        (mode, capture)
    }

    /// A mode whose extension is bound to a live `ctx.ui` bridge (E3
    /// slice 6a): the sink the runner forwards into is the same Arc the
    /// mode's pump drains — `extensions::load`'s bind step applies the UI
    /// surface before `session_start` (`agent-session.ts:3429-3449`).
    #[cfg(feature = "ext-js")]
    fn fixture_with_extension_ui(
        dir: &std::path::Path,
        source: &str,
    ) -> (
        InteractiveMode,
        FrameCapture,
        Arc<crate::modes::interactive::extension_ui::ExtensionUiBridge>,
    ) {
        let bridge = Arc::new(crate::modes::interactive::extension_ui::ExtensionUiBridge::new());
        let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
            cwd: dir.to_string_lossy().to_string(),
            mode: "tui".to_string(),
            settings: serde_json::json!({}),
            ..Default::default()
        })
        .expect("runner");
        runner
            .load_extension("<test-0>", Some(source))
            .expect("load");
        runner.bind().expect("bind");
        runner.set_ui_context(Arc::clone(&bridge) as Arc<dyn pi_ext_js::UiSink>, "tui");
        let stream: Arc<dyn pi_agent::types::StreamFn> = Arc::new(ScriptedStream {
            calls: AtomicUsize::new(0),
            file_path: "unused".to_string(),
        });
        let terminal = CaptureTerminal::with_size(TerminalSize {
            columns: 100,
            rows: 40,
        });
        let capture = terminal.capture();
        let options = InteractiveOptions {
            auth_path: Some(dir.join("auth.json")),
            extension_ui: Some(Arc::clone(&bridge)),
            ..InteractiveOptions::default()
        };
        let mut runtime = test_runtime(dir, stream);
        runtime.extension_runner = Some(Arc::new(std::sync::Mutex::new(runner)));
        let mode = InteractiveMode::new(&mut runtime, &options, Box::new(terminal));
        (mode, capture, bridge)
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

    /// Back-to-back statuses REPLACE the chat-tail pair in place
    /// (`showStatus`'s identity check, `interactive-mode.ts:3964-3984`) —
    /// the user-reported stacking: every thinking-level press used to append
    /// another line. The count stays fixed and the tracked pair keeps
    /// matching the current tail rows.
    #[test]
    fn consecutive_statuses_replace_the_chat_tail_pair() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        let before = mode.chat_len();
        mode.show_status("Thinking level: minimal");
        assert_eq!(mode.chat_len(), before + 2, "spacer + text appended");
        let (_, text_addr) = mode.last_status_pair.expect("pair tracked");
        let tail_index = mode.chat_len();
        assert_eq!(
            mode.chat_row_addr(tail_index - 1),
            Some(text_addr),
            "the pair tracks the tail text row"
        );
        mode.show_status("Thinking level: low");
        mode.show_status("Thinking level: medium");
        assert_eq!(
            mode.chat_len(),
            before + 2,
            "back-to-back statuses replace the pair, not stack rows"
        );
        let (spacer_addr, text_addr) = mode.last_status_pair.expect("pair tracked");
        let tail_index = mode.chat_len();
        assert_eq!(mode.chat_row_addr(tail_index - 1), Some(text_addr));
        assert_eq!(mode.chat_row_addr(tail_index - 2), Some(spacer_addr));
    }

    /// Any other row added to the chat breaks the tail pair: the next status
    /// appends a fresh pair (upstream compares the object identity of the
    /// last two children, `interactive-mode.ts:3968-3972`).
    #[test]
    fn interleaved_chat_row_breaks_the_status_pair() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        let before = mode.chat_len();
        mode.show_status("first");
        mode.add_to_chat(Box::new(Text::new("interleaved", 1, 0)));
        mode.show_status("second");
        assert_eq!(
            mode.chat_len(),
            before + 5,
            "pair (2) + interleaved row (1) + fresh pair (2)"
        );
        let (spacer_addr, text_addr) = mode.last_status_pair.expect("pair tracked");
        let tail_index = mode.chat_len();
        assert_eq!(mode.chat_row_addr(tail_index - 1), Some(text_addr));
        assert_eq!(mode.chat_row_addr(tail_index - 2), Some(spacer_addr));
    }

    /// The user-reported repro through the real key path: cycling the
    /// thinking level repeatedly appends exactly one status pair
    /// (`cycleThinkingLevel` → `showStatus`, `interactive-mode.ts:4711-4721`).
    #[test]
    fn thinking_level_cycle_renders_one_status_line() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        assert!(mode.model.reasoning, "the fixture model supports thinking");
        let before = mode.chat_len();
        for _ in 0..6 {
            mode.cycle_thinking_level();
        }
        mode.tui.render_now(false);
        assert_eq!(mode.chat_len(), before + 2, "the cycle stacks nothing");
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Thinking level:"), "{plain}");
    }

    /// `showError` (`interactive-mode.ts:4809-4813`): a leading spacer, the
    /// `Error: ` prefix, and — unlike `showStatus` — no coalescing: each
    /// error appends its own rows.
    #[test]
    fn show_error_renders_the_prefix_and_never_coalesces() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        let before = mode.chat_len();
        mode.show_error("boom");
        mode.tui.render_now(false);
        assert_eq!(mode.chat_len(), before + 2);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Error: boom"), "{plain}");
        mode.show_error("again");
        assert_eq!(mode.chat_len(), before + 4, "errors never coalesce");
    }

    /// The in-session `/resume` dialog REPLACES the transcript (upstream's
    /// `renderCurrentSessionState` clears the chat before replaying,
    /// `interactive-mode.ts:2337-2347` via `rebindCurrentSession`) — the
    /// resumed entries do not append below the old chat.
    #[test]
    fn resumed_session_replaces_the_previous_transcript() {
        use pi_ai::types::{UserContent, UserMessage};

        let dir = tempfile::tempdir().expect("tempdir");
        let make_session = |dir: &std::path::Path, prompts: &[&str]| {
            let mut session =
                SessionManager::create(dir, None, &NewSessionOptions::default()).expect("session");
            for prompt in prompts {
                session
                    .append_message(AgentMessage::User(UserMessage {
                        content: UserContent::Text((*prompt).to_string()),
                        timestamp: 0,
                    }))
                    .expect("append user");
            }
            session.session_file().expect("session file").to_path_buf()
        };
        let path_a = make_session(dir.path(), &["first session alpha", "first session beta"]);
        let path_b = make_session(dir.path(), &["second session gamma"]);

        let options = InteractiveOptions {
            auth_path: Some(dir.path().join("auth.json")),
            ..InteractiveOptions::default()
        };
        let new_stream = || {
            Arc::new(ScriptedStream {
                calls: AtomicUsize::new(0),
                file_path: "unused".to_string(),
            }) as Arc<dyn pi_agent::types::StreamFn>
        };

        // The baseline: how many rows a fresh boot of session B replays.
        let mut runtime_b = test_runtime(dir.path(), new_stream());
        runtime_b.session = SessionManager::open(&path_b, None, None).expect("reopen b");
        let mut mode_b = InteractiveMode::new(
            &mut runtime_b,
            &options,
            Box::new(CaptureTerminal::with_size(TerminalSize {
                columns: 100,
                rows: 40,
            })),
        );
        mode_b.render_initial_messages();
        let fresh_b_rows = mode_b.chat_len();
        assert!(fresh_b_rows > 0, "session b replays its user message");

        // Boot over session A (a longer transcript), then resume B in place.
        let mut runtime = test_runtime(dir.path(), new_stream());
        runtime.session = SessionManager::open(&path_a, None, None).expect("reopen a");
        let mut mode = InteractiveMode::new(
            &mut runtime,
            &options,
            Box::new(CaptureTerminal::with_size(TerminalSize {
                columns: 100,
                rows: 40,
            })),
        );
        mode.render_initial_messages();
        let len_a = mode.chat_len();
        assert!(len_a > fresh_b_rows, "the fixtures must differ in size");

        mode.switch_session(&path_b);
        assert_eq!(
            mode.chat_len(),
            fresh_b_rows + 2,
            "the resumed transcript REPLACED the old chat (plus the `Resumed session` status pair)"
        );
        let (spacer_addr, text_addr) = mode.last_status_pair.expect("pair tracked");
        let tail_index = mode.chat_len();
        assert_eq!(mode.chat_row_addr(tail_index - 1), Some(text_addr));
        assert_eq!(mode.chat_row_addr(tail_index - 2), Some(spacer_addr));
    }

    /// An extension's `ctx.ui.select` mounts the option dialog over the
    /// bridge and the picked option resolves the promise
    /// (`showExtensionSelector`, `interactive-mode.ts:2858-2905`).
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_select_mounts_and_resolves() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const picked = await ctx.ui.select("Pick one", ["A", "B"]);
    pi.appendEntry("picked", { value: picked ?? null });
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        // `session_start` fires after the bind; the handler's select
        // blocks the dispatch until the dialog answers — run it off the
        // pump, like the real session_start emission during boot.
        let runner = mode.extension_runner.clone().expect("runner");
        let emitter = std::thread::spawn(move || {
            runner
                .lock()
                .expect("runner")
                .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
                .expect("emit");
        });
        for _ in 0..200 {
            mode.pump();
            if !mode.extension_ui_dialogs.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let dialog = mode.extension_ui_dialogs.last().expect("dialog mounted");
        assert_eq!(dialog.kind, ExtensionUiKind::Select);
        assert!(mode.suspended_editor.is_some(), "the editor is parked");
        // Pick "B" through the component's event mailbox.
        match &dialog.events {
            ExtensionUiDialogEvents::Selector(events) => {
                events
                    .lock()
                    .expect("events")
                    .push(ExtensionSelectorEvent::Selected("B".to_string()));
            }
            _ => panic!("selector events"),
        }
        mode.pump();
        // The dispatch settled with the picked value.
        emitter.join().expect("emit thread");
        let entries = mode
            .extension_runner
            .as_ref()
            .expect("runner")
            .lock()
            .expect("runner")
            .core()
            .lock()
            .expect("core")
            .entries
            .clone();
        let picked = entries
            .iter()
            .find(|(custom_type, _)| custom_type == "picked")
            .map(|(_, data)| data.clone());
        assert_eq!(picked, Some(serde_json::json!({ "value": "B" })));
        assert!(mode.extension_ui_dialogs.is_empty(), "the dialog closed");
        assert!(mode.suspended_editor.is_none(), "editor restored");
        mode.shutdown().expect("shutdown");
    }

    /// `ctx.ui.input` mounts the text dialog and real keystrokes reach it
    /// through the focused editor slot; enter resolves the line
    /// (`showExtensionInput`, `interactive-mode.ts:2921-2950`).
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_input_mounts_and_submits() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const value = await ctx.ui.input("Your name");
    pi.appendEntry("named", { value: value ?? null });
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        let emitter = std::thread::spawn(move || {
            runner
                .lock()
                .expect("runner")
                .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
                .expect("emit");
        });
        for _ in 0..200 {
            mode.pump();
            if !mode.extension_ui_dialogs.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let dialog = mode.extension_ui_dialogs.last().expect("dialog mounted");
        assert_eq!(dialog.kind, ExtensionUiKind::Input);
        // Real keystrokes: the tui routes them to the focused dialog.
        mode.feed_stdin_bytes(b"h");
        mode.feed_stdin_bytes(b"i");
        mode.feed_stdin_bytes(b"\r");
        mode.pump();
        emitter.join().expect("emit thread");
        let entries = mode
            .extension_runner
            .as_ref()
            .expect("runner")
            .lock()
            .expect("runner")
            .core()
            .lock()
            .expect("core")
            .entries
            .clone();
        let named = entries
            .iter()
            .find(|(custom_type, _)| custom_type == "named")
            .map(|(_, data)| data.clone());
        assert_eq!(named, Some(serde_json::json!({ "value": "hi" })));
        assert!(mode.extension_ui_dialogs.is_empty(), "the dialog closed");
        mode.shutdown().expect("shutdown");
    }

    /// `ctx.ui.notify` lands as chat lines by type
    /// (`showExtensionNotify`, `interactive-mode.ts:3061-3071`).
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_notify_renders_by_type() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.notify("plain note");
    ctx.ui.notify("careful", "warning");
    ctx.ui.notify("bad", "error");
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        mode.tui.start().expect("tui start");
        let runner = mode.extension_runner.clone().expect("runner");
        let emitter = std::thread::spawn(move || {
            runner
                .lock()
                .expect("runner")
                .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
                .expect("emit");
        });
        for _ in 0..200 {
            mode.pump();
            if emitter.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        emitter.join().expect("emit thread");
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("plain note"), "{plain}");
        assert!(plain.contains("Warning: careful"), "{plain}");
        assert!(plain.contains("Error: bad"), "{plain}");
        assert!(
            mode.extension_ui_dialogs.is_empty(),
            "notify never mounts a dialog"
        );
        mode.shutdown().expect("shutdown");
    }

    /// `ctx.ui.setStatus` feeds the footer's third line
    /// (`setExtensionStatus`, `interactive-mode.ts:2440-2443`, rendered at
    /// `footer.ts:289-298`): values render sorted by key, sanitized, and an
    /// undefined text deletes the key.
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_set_status_renders_in_the_footer() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.setStatus("beta", "second  status");
    ctx.ui.setStatus("alpha", "first\tstatus");
    pi.appendEntry("marked", {});
  });
  pi.on("session_end", (_event, ctx) => {
    ctx.ui.setStatus("alpha", undefined);
    pi.appendEntry("cleared", {});
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        // The fire-and-forget statuses queue during the emit; a pump after it
        // settles drains them into the footer.
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        mode.pump();
        let footer = mode
            .downcast_tree::<FooterComponent>(HostSlot::Footer)
            .expect("footer");
        assert_eq!(
            footer
                .data
                .extension_statuses
                .get("alpha")
                .map(String::as_str),
            Some("first\tstatus"),
            "the raw text is stored unsanitized"
        );
        let lines = footer.render_lines(100);
        assert_eq!(lines.len(), 3, "the status line is appended");
        assert_eq!(
            pi_tui::ansi::strip_terminal_sequences(&lines[2]),
            "first status second status",
            "sorted by key, tab collapsed"
        );
        // An undefined text deletes the key (`footer-data-provider.ts:146-152`).
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_end" }))
            .expect("emit");
        mode.pump();
        let footer = mode
            .downcast_tree::<FooterComponent>(HostSlot::Footer)
            .expect("footer");
        assert!(
            !footer.data.extension_statuses.contains_key("alpha"),
            "undefined deletes"
        );
        // `beta` was never cleared, so its status keeps the line — but only
        // its own text renders now.
        assert_eq!(
            footer.render_lines(100).len(),
            3,
            "the remaining status keeps the line"
        );
        mode.shutdown().expect("shutdown");
    }

    // --------------------------------------------- small ctx.ui surfaces (E3 slice 7)

    /// `setWidget`'s lines arm (`interactive-mode.ts:2538-2578`): the above
    /// host leads with a spacer, the below host never spacers, the key
    /// spans both placements, and `undefined` removes.
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_widgets_render_above_and_below_the_editor() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.setWidget("alpha", ["one", "two"]);
    ctx.ui.setWidget("beta", ["three"], { placement: "belowEditor" });
    pi.appendEntry("marked", {});
  });
  pi.on("session_end", (_event, ctx) => {
    ctx.ui.setWidget("alpha", undefined);
    ctx.ui.setWidget("beta", undefined);
    pi.appendEntry("cleared", {});
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        mode.pump();

        let render_widget_host = |mode: &mut InteractiveMode, slot: HostSlot| {
            mode.downcast_tree::<pi_tui::component::Container>(slot)
                .expect("widget host")
                .render(100)
        };
        let above = render_widget_host(&mut mode, HostSlot::WidgetAbove);
        let above_text: Vec<String> = above
            .iter()
            .map(|line| {
                pi_tui::ansi::strip_terminal_sequences(line)
                    .trim()
                    .to_string()
            })
            .collect();
        // `renderWidgetContainer(above)`: the leading spacer then the rows.
        assert_eq!(
            above_text,
            ["", "one", "two"],
            "the above host leads with a spacer"
        );
        let below = render_widget_host(&mut mode, HostSlot::WidgetBelow);
        let below_text: Vec<String> = below
            .iter()
            .map(|line| {
                pi_tui::ansi::strip_terminal_sequences(line)
                    .trim()
                    .to_string()
            })
            .collect();
        assert_eq!(below_text, ["three"], "the below host never spacers");

        // The key removes from both placements; `undefined` removes.
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_end" }))
            .expect("emit");
        mode.pump();
        let above = render_widget_host(&mut mode, HostSlot::WidgetAbove);
        assert_eq!(above.len(), 1, "the empty above host keeps only its spacer");
        let below = render_widget_host(&mut mode, HostSlot::WidgetBelow);
        assert!(below.is_empty(), "the below host renders nothing");
        mode.shutdown().expect("shutdown");
    }

    /// `setWidget`'s lines cap at `MAX_WIDGET_LINES` with the muted
    /// truncation line (`interactive-mode.ts:2563-2568`, `:2626`).
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_widgets_cap_at_the_line_limit() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.setWidget("long", ["l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9", "l10", "l11", "l12"]);
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        mode.pump();
        let lines = mode
            .downcast_tree::<pi_tui::component::Container>(HostSlot::WidgetAbove)
            .expect("widget host")
            .render(100);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                pi_tui::ansi::strip_terminal_sequences(line)
                    .trim()
                    .to_string()
            })
            .collect();
        assert_eq!(
            text,
            [
                "",
                "l1",
                "l2",
                "l3",
                "l4",
                "l5",
                "l6",
                "l7",
                "l8",
                "l9",
                "l10",
                "... (widget truncated)"
            ],
            "ten rows plus the muted truncation line, past the leading spacer"
        );
        mode.shutdown().expect("shutdown");
    }

    /// `setWidget`'s factory arm (`interactive-mode.ts:2570-2573`): the
    /// adopted component renders through the borrowed proxy.
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_widget_factory_adopts_a_component() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.setWidget("clock", (_tui, _theme) => ({
      render: (width) => ["CLOCK " + String(width)],
      invalidate: () => {},
    }));
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        mode.pump();
        let lines = mode
            .downcast_tree::<pi_tui::component::Container>(HostSlot::WidgetAbove)
            .expect("widget host")
            .render(100);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                pi_tui::ansi::strip_terminal_sequences(line)
                    .trim()
                    .to_string()
            })
            .collect();
        assert_eq!(
            text,
            ["", "CLOCK 100"],
            "the adopted widget renders at the host width, past the leading spacer"
        );
        mode.shutdown().expect("shutdown");
    }

    /// `setFooter`/`setHeader` (`interactive-mode.ts:2664-2730`): the
    /// adopted component occupies the slot, the built-ins come back on
    /// `undefined`, and the displaced built-in keeps taking refresh writes.
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_footer_and_header_swap_and_restore() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.setFooter((_tui, _theme, footerData) => ({
      render: (_width) => ["CUSTOM FOOTER " + String(footerData.getGitBranch() === undefined)],
      invalidate: () => {},
    }));
    ctx.ui.setHeader((_tui, _theme) => ({
      render: (_width) => ["CUSTOM HEADER"],
      invalidate: () => {},
    }));
    pi.appendEntry("marked", {});
  });
  pi.on("session_end", (_event, ctx) => {
    ctx.ui.setFooter(undefined);
    ctx.ui.setHeader(undefined);
    pi.appendEntry("cleared", {});
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        mode.pump();
        let footer = mode
            .downcast_tree::<ComponentProxy>(HostSlot::Footer)
            .expect("the custom footer occupies the slot");
        assert!(
            pi_tui::ansi::strip_terminal_sequences(&footer.render(100).join("\n"))
                .contains("CUSTOM FOOTER"),
            "{:?}",
            footer.render(100)
        );
        let header = mode
            .downcast_tree::<ComponentProxy>(HostSlot::Header)
            .expect("the custom header occupies the slot");
        assert!(
            pi_tui::ansi::strip_terminal_sequences(&header.render(100).join("\n"))
                .contains("CUSTOM HEADER")
        );

        // The displaced built-ins keep taking the refresh writes — the
        // restore shows current data.
        let footer_component = mode
            .custom_footer
            .as_mut()
            .and_then(|chrome| chrome.displaced.as_any_mut())
            .and_then(|any| any.downcast_mut::<FooterComponent>())
            .expect("the builtin footer stays alive");
        footer_component.data.model_id = "switched".to_string();

        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_end" }))
            .expect("emit");
        mode.pump();
        let footer = mode
            .downcast_tree::<FooterComponent>(HostSlot::Footer)
            .expect("the built-in footer is back");
        assert_eq!(
            footer.data.model_id, "switched",
            "the displaced built-in kept taking the writes"
        );
        let header = mode
            .downcast_tree::<BuiltInHeader>(HostSlot::Header)
            .expect("the built-in header is back");
        assert!(
            !header.render(100).join("\n").contains("CUSTOM HEADER"),
            "the built-in header is back"
        );
        mode.shutdown().expect("shutdown");
    }

    /// The working-status family (`interactive-mode.ts:2501-2533`,
    /// `:2781-2789`): the message, frames, and visibility drive the live
    /// indicator, and `setToolsExpanded` carries the flag, the status line,
    /// and the unchanged-flag no-op (`:4745-4761`).
    #[test]
    fn working_status_state_and_tools_expanded_drive_the_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _terminal) = themed_fixture(dir.path());
        mode.set_working_status(Some(StatusIndicator::working("Working...")));

        mode.set_working_message(Some("Custom".to_string()));
        let message = mode
            .status_handle
            .lock()
            .expect("status")
            .as_ref()
            .map(|indicator| indicator.render_in_border(60))
            .expect("the working indicator is live");
        assert!(
            message.contains("Custom"),
            "the live indicator shows the working message: {message}"
        );

        mode.set_working_indicator(Some(pi_tui::components::loader::LoaderIndicator {
            frames: vec!["x".to_string()],
            interval_ms: 120,
        }));
        let interval = mode
            .status_handle
            .lock()
            .expect("status")
            .as_ref()
            .map(StatusIndicator::interval_ms)
            .expect("the working indicator is live");
        assert_eq!(interval, 120, "the indicator options reached the loader");

        // false clears the working indicator; true re-shows one only while
        // a run streams (`:2503-2510`).
        mode.set_working_visible(false);
        assert!(mode.status_handle.lock().expect("status").is_none());
        mode.set_working_visible(true);
        assert!(mode.status_handle.lock().expect("status").is_none());

        let before = mode.chat_len();
        mode.set_tools_expanded(true);
        assert!(mode.tool_output_expanded);
        assert_eq!(mode.chat_len(), before + 2, "the status pair appends once");
        mode.set_tools_expanded(true);
        assert_eq!(mode.chat_len(), before + 2, "the unchanged flag is a no-op");
        let chat = mode.chat_mut().render(100).join("\n");
        assert!(
            pi_tui::ansi::strip_terminal_sequences(&chat).contains("Tool output: expanded"),
            "the status line names the state"
        );
        mode.set_tools_expanded(false);
        assert!(!mode.tool_output_expanded);
        mode.shutdown().expect("shutdown");
    }

    /// The terminal-input fold (E3 slice 7): registered listeners transform
    /// or consume chunks before the editor sees them, and the count sync
    /// arms the dispatch.
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_terminal_input_folds_before_the_editor() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.onTerminalInput((data) => (data === "q" ? { data: "z" } : undefined));
    pi.appendEntry("registered", {});
  });
  pi.on("session_end", (_event, ctx) => {
    ctx.ui.onTerminalInput((data) => (data === "z" ? { consume: true } : undefined));
    pi.appendEntry("second", {});
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        mode.tui.start().expect("tui start");
        let runner = mode.extension_runner.clone().expect("runner");
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        mode.pump();
        assert_eq!(mode.terminal_input_listeners, 1, "the count sync armed");

        mode.dispatch_stdin_events(vec![pi_tui::stdin_buffer::StdinEvent::Data(
            "q".to_string(),
        )]);
        let text = mode
            .editor_host_mut()
            .expect("editor host")
            .editor
            .get_text();
        assert_eq!(text, "z", "the transform reached the editor");

        // The second listener consumes the transformed chunk.
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_end" }))
            .expect("emit");
        mode.pump();
        mode.dispatch_stdin_events(vec![pi_tui::stdin_buffer::StdinEvent::Data(
            "q".to_string(),
        )]);
        let text = mode
            .editor_host_mut()
            .expect("editor host")
            .editor
            .get_text();
        assert_eq!(text, "z", "the consumed chunk never reached the editor");
        mode.shutdown().expect("shutdown");
    }

    /// Autocomplete wrappers (E3 slice 8): `addAutocompleteProvider` arms
    /// the chain — the wrapper's suggestions (built over the base chain's
    /// result) open the popup, Tab applies through the chain (the wrapper
    /// defines no `applyCompletion`, so the base's application runs), and
    /// `resetExtensionUI` restores the base-only chain.
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_autocomplete_wrappers_extend_the_chain() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.addAutocompleteProvider((base) => ({
      triggerCharacters: ["!"],
      getSuggestions: async (lines, cursorLine, cursorCol, opts) => {
        const baseResult = await base.getSuggestions(lines, cursorLine, cursorCol, opts);
        return {
          items: [
            { value: "ext-item", label: "ExtItem", description: baseResult ? "base-ok" : "nobase" },
          ],
          prefix: "/",
        };
      },
    }));
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        mode.tui.start().expect("tui start");
        let runner = mode.extension_runner.clone().expect("runner");
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        mode.pump();
        assert_eq!(
            mode.autocomplete_wrappers.len(),
            1,
            "the wrapper registered"
        );

        // Typing `/` at the start of the message triggers the chain: the
        // wrapper's item opens the popup and the base chain's result
        // crossed as data (the description proves it was non-null).
        mode.dispatch_stdin_events(vec![pi_tui::stdin_buffer::StdinEvent::Data(
            "/".to_string(),
        )]);
        let rendered = {
            let editor = &mut mode.editor_host_mut().expect("editor host").editor;
            assert!(editor.is_showing_autocomplete(), "the wrapper suggests");
            pi_tui::ansi::strip_terminal_sequences(&editor.render(100).join("\n"))
        };
        assert!(rendered.contains("ExtItem"), "{rendered}");
        assert!(
            rendered.contains("base-ok"),
            "the base chain crossed as data: {rendered}"
        );

        // Tab applies through the chain: no JS `applyCompletion`, so the
        // base's slash-command application runs (`/ext-item `).
        mode.dispatch_stdin_events(vec![pi_tui::stdin_buffer::StdinEvent::Data(
            "\t".to_string(),
        )]);
        let text = mode
            .editor_host_mut()
            .expect("editor host")
            .editor
            .get_text();
        assert_eq!(
            text, "/ext-item ",
            "the base application completed the token"
        );

        // The reset restores the base-only chain: re-typing `/` suggests
        // the commands again without the wrapper's item.
        let backspaces: Vec<pi_tui::stdin_buffer::StdinEvent> = std::iter::repeat_n(
            pi_tui::stdin_buffer::StdinEvent::Data("\x7f".to_string()),
            12,
        )
        .collect();
        mode.dispatch_stdin_events(backspaces);
        mode.reset_extension_ui_dialogs();
        assert!(
            mode.autocomplete_wrappers.is_empty(),
            "the reset cleared the chain"
        );
        mode.dispatch_stdin_events(vec![pi_tui::stdin_buffer::StdinEvent::Data(
            "/".to_string(),
        )]);
        let rendered = {
            let editor = &mut mode.editor_host_mut().expect("editor host").editor;
            assert!(
                editor.is_showing_autocomplete(),
                "the base chain still suggests after the reset"
            );
            pi_tui::ansi::strip_terminal_sequences(&editor.render(100).join("\n"))
        };
        assert!(
            !rendered.contains("ExtItem"),
            "the reset chain answers without the wrapper: {rendered}"
        );
        mode.shutdown().expect("shutdown");
    }

    /// The extension editor component (E3 slice 9): the delegate renders in
    /// the slot and takes every key (`this.editor` IS the component), the
    /// text ops route through it, the wired `onSubmit` flows through the
    /// submit pipeline, and the reset restores the built-in editor with the
    /// component's text (`setCustomEditorComponent` +
    /// `resetExtensionUI`, `interactive-mode.ts:2979-3055`/`:2611`).
    #[test]
    fn extension_ui_editor_component_takes_the_editor_slot() {
        const EXT: &str = r#"
export default function (pi) {
  let text = "";
  let component;
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.setEditorComponent(() => {
      component = {
        render: () => ["[EXT] " + text],
        invalidate: () => {},
        getText: () => text,
        setText: (next) => { text = next; },
        handleInput: (data) => {
          for (let i = 0; i < data.length; i++) {
            const code = data.charCodeAt(i);
            if (code === 13) {
              if (component.onSubmit) component.onSubmit(text);
            } else if (code === 127) {
              text = text.slice(0, -1);
            } else {
              text += data[i];
            }
          }
          return true;
        },
      };
      return component;
    });
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        mode.tui.start().expect("tui start");
        let runner = mode.extension_runner.clone().expect("runner");
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        mode.pump();
        assert!(
            mode.extension_editor.lock().expect("extension editor").is_some(),
            "the delegate mounted"
        );
        assert!(
            mode.extension_editor_status_row,
            "the working status moved to the host row"
        );

        // The delegate takes the keys: typed characters land in the
        // component, the render shows it, and the built-in editor stays
        // empty (nothing leaks underneath).
        mode.dispatch_stdin_events(vec![pi_tui::stdin_buffer::StdinEvent::Data(
            "h".to_string(),
        )]);
        mode.dispatch_stdin_events(vec![pi_tui::stdin_buffer::StdinEvent::Data(
            "i".to_string(),
        )]);
        let rendered = pi_tui::ansi::strip_terminal_sequences(
            &mode
                .editor_host_mut()
                .expect("editor host")
                .render(100)
                .join("\n"),
        );
        assert!(rendered.contains("[EXT] hi"), "{rendered}");
        assert_eq!(
            mode.editor_host_mut().expect("editor host").editor.get_text(),
            "",
            "the built-in editor stays empty under the delegate"
        );
        // The snapshot (what `getEditorText` serves) mirrors the component.
        assert_eq!(
            mode.ui_snapshot.lock().expect("ui snapshot").editor_text,
            "hi"
        );

        // `setEditorText` routes through the delegate's `setText`.
        mode.set_editor_text("hey");
        let rendered = pi_tui::ansi::strip_terminal_sequences(
            &mode
                .editor_host_mut()
                .expect("editor host")
                .render(100)
                .join("\n"),
        );
        assert!(rendered.contains("[EXT] hey"), "{rendered}");

        // The wired `onSubmit` flows through the submit pipeline: "/exit"
        // sets the exit flag, and the built-in editor's text was NOT
        // cleared (the component owns its text lifecycle).
        mode.set_editor_text("/exit");
        mode.dispatch_stdin_events(vec![pi_tui::stdin_buffer::StdinEvent::Data(
            "\r".to_string(),
        )]);
        mode.pump();
        mode.pump();
        assert!(mode.exiting, "the component's submit ran the pipeline");
        assert_eq!(
            mode.editor_host_mut().expect("editor host").editor.get_text(),
            "",
            "the built-in editor was empty all along"
        );

        // The reset restores the built-in editor with the component's text
        // and unmounts the status row.
        mode.reset_extension_ui_dialogs();
        assert!(
            mode.extension_editor.lock().expect("extension editor").is_none(),
            "the reset dropped the delegate"
        );
        assert!(
            !mode.extension_editor_status_row,
            "the status row unmounted"
        );
        assert_eq!(
            mode.editor_host_mut().expect("editor host").editor.get_text(),
            "/exit",
            "the component's text moved back into the built-in editor"
        );
        let rendered = pi_tui::ansi::strip_terminal_sequences(
            &mode
                .editor_host_mut()
                .expect("editor host")
                .render(100)
                .join("\n"),
        );
        assert!(
            !rendered.contains("[EXT]"),
            "the built-in editor renders after the reset: {rendered}"
        );
        mode.shutdown().expect("shutdown");
    }

    /// `resetExtensionUI`'s slice-7 clears (`interactive-mode.ts:2603-2622`):
    /// the listeners, the footer/header swaps, the widgets, and the working
    /// state all reset while the deferred chunks drain.
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_reset_restores_the_builtins() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    ctx.ui.setWidget("alpha", ["one"]);
    ctx.ui.setFooter((_tui, _theme, _footerData) => ({
      render: (_width) => ["CUSTOM FOOTER"],
      invalidate: () => {},
    }));
    ctx.ui.setHeader((_tui, _theme) => ({
      render: (_width) => ["CUSTOM HEADER"],
      invalidate: () => {},
    }));
    ctx.ui.onTerminalInput(() => undefined);
    ctx.ui.setWorkingMessage("Custom");
    ctx.ui.setHiddenThinkingLabel("custom");
    pi.appendEntry("marked", {});
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        runner
            .lock()
            .expect("runner")
            .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
            .expect("emit");
        mode.pump();
        assert_eq!(mode.terminal_input_listeners, 1);
        assert_eq!(mode.working_message.as_deref(), Some("Custom"));
        assert_eq!(mode.hidden_thinking_label, "custom");
        mode.reset_extension_ui_dialogs();
        assert_eq!(mode.terminal_input_listeners, 0, "the listeners cleared");
        assert!(
            mode.deferred_terminal_input.is_empty(),
            "the deferred chunks drained"
        );
        assert!(mode.extension_widgets_above.is_empty());
        assert!(mode.extension_widgets_below.is_empty());
        assert!(mode.custom_footer.is_none());
        assert!(mode.custom_header.is_none());
        assert!(
            mode.downcast_tree::<FooterComponent>(HostSlot::Footer)
                .is_some(),
            "the built-in footer is back"
        );
        assert!(
            mode.downcast_tree::<BuiltInHeader>(HostSlot::Header)
                .is_some(),
            "the built-in header is back"
        );
        assert_eq!(mode.working_message, None);
        assert!(mode.working_visible);
        assert_eq!(mode.hidden_thinking_label, "Thinking...");
        mode.shutdown().expect("shutdown");
    }

    /// A mounted extension dialog reports the `blocked` program status over
    /// OSC 7501 (`showExtensionSelector`'s default `blocked` →
    /// `showTopExtensionDialog`, `interactive-mode.ts:2864-2888/:2853`), and
    /// closing it reports the resting state again. The sequence reaches the
    /// terminal writer through the mode's capture terminal.
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_dialog_reports_blocked_program_status_over_osc_7501() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const value = await ctx.ui.select("Pick", ["A", "B"]);
    pi.appendEntry("picked", { value: value ?? null });
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        mode.tui.start().expect("tui start");
        // Answer the OSC 7501 negotiation so reports write to the capture.
        mode.feed_stdin_bytes(b"\x1b]7501;?\x1b\\");
        let runner = mode.extension_runner.clone().expect("runner");
        let emitter = std::thread::spawn(move || {
            runner
                .lock()
                .expect("runner")
                .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
                .expect("emit");
        });
        for _ in 0..200 {
            mode.pump();
            if !mode.extension_ui_dialogs.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(mode.extension_ui_dialogs.len(), 1, "the select mounted");
        let written = capture.output();
        // base64("Pick") = UGljaw== — the dialog title is the blocked message.
        assert!(
            written.contains("\x1b]7501;state=blocked:app=pi:kind=question:msg=UGljaw==\x1b\\"),
            "{written:?}"
        );
        // Enter picks the first option; the close clears the source.
        mode.feed_stdin_bytes(b"\r");
        mode.pump();
        emitter.join().expect("emit thread");
        let written = capture.output();
        assert!(
            written.contains("\x1b]7501;state=idle:app=pi\x1b\\"),
            "the blocked source cleared on close: {written:?}"
        );
        let entries = mode
            .extension_runner
            .as_ref()
            .expect("runner")
            .lock()
            .expect("runner")
            .core()
            .lock()
            .expect("core")
            .entries
            .clone();
        let picked = entries
            .iter()
            .find(|(custom_type, _)| custom_type == "picked")
            .map(|(_, data)| data.clone());
        assert_eq!(
            picked,
            Some(serde_json::json!({ "value": "A" })),
            "the dialog still answered"
        );
        mode.shutdown().expect("shutdown");
    }

    /// `ctx.ui.custom` with `overlay: true` mounts ON TOP of the existing
    /// content through `show_overlay` (`showExtensionCustom`'s overlay arm,
    /// `interactive-mode.ts:3089/:3139-3157`) — never through the editor
    /// slot's dialog stack — and `done()` removes it, clearing its blocked
    /// program-status source (`removeOverlay`, `:3113-3120`).
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_overlay_custom_mounts_and_done_removes() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const value = await ctx.ui.custom(
      (_tui, _theme, _kb, done) => ({
        render: (width) => ["OVERLAY BODY " + width],
        invalidate: () => {},
        handleInput: (data) => {
          if (data === "x") {
            done("overlay-value");
            return true;
          }
          return false;
        },
      }),
      {
        overlay: true,
        overlayOptions: { width: 40 },
        blocked: { kind: "question", message: "Blocking" },
      },
    );
    pi.appendEntry("picked", { value: value ?? null });
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        mode.tui.start().expect("tui start");
        // Answer the OSC 7501 negotiation so reports write to the capture.
        mode.feed_stdin_bytes(b"\x1b]7501;?\x1b\\");
        let runner = mode.extension_runner.clone().expect("runner");
        let emitter = std::thread::spawn(move || {
            runner
                .lock()
                .expect("runner")
                .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
                .expect("emit");
        });
        for _ in 0..200 {
            mode.pump();
            if !mode.extension_overlays.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        emitter.join().expect("emit thread");
        // The overlay registry, not the dialog stack; the editor slot keeps
        // its occupant.
        assert_eq!(mode.extension_overlays.len(), 1, "the overlay mounted");
        assert!(
            mode.extension_ui_dialogs.is_empty(),
            "overlays never enter the dialog stack"
        );
        assert_eq!(mode.tui.overlay_count(), 1, "mounted in the tui");
        assert!(mode.suspended_editor.is_none(), "the editor was not parked");
        // The blocked status rode the request; the numbered source reports
        // it (base64("Blocking") = QmxvY2tpbmc=).
        let written = capture.output();
        assert!(
            written.contains("\x1b]7501;state=blocked:app=pi:kind=question:msg=QmxvY2tpbmc=\x1b\\"),
            "{written:?}"
        );
        // Input reaches the overlay through the captured focus; done()
        // removes it and clears the source.
        mode.feed_stdin_bytes(b"x");
        mode.pump();
        assert!(mode.extension_overlays.is_empty(), "done removed it");
        assert_eq!(mode.tui.overlay_count(), 0, "the tui dropped the overlay");
        let written = capture.output();
        assert!(
            written.contains("\x1b]7501;state=idle:app=pi\x1b\\"),
            "the blocked source cleared: {written:?}"
        );
        let entries = mode
            .extension_runner
            .as_ref()
            .expect("runner")
            .lock()
            .expect("runner")
            .core()
            .lock()
            .expect("core")
            .entries
            .clone();
        let picked = entries
            .iter()
            .find(|(custom_type, _)| custom_type == "picked")
            .map(|(_, data)| data.clone());
        assert_eq!(
            picked,
            Some(serde_json::json!({ "value": "overlay-value" }))
        );
        mode.shutdown().expect("shutdown");
    }

    /// Reset removes the extension overlays NEWEST-FIRST, the same way
    /// `done()` does, but leaves the promises pending
    /// (`resetExtensionUI`, `interactive-mode.ts:2600-2602`): no entry is
    /// appended and no `customDone` ever crosses.
    #[cfg(feature = "ext-js")]
    #[test]
    fn reset_removes_overlays_newest_first_leaving_promises_pending() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const first = ctx.ui.custom(
      (_tui, _theme, _kb, doneA) => ({
        render: (width) => ["FIRST " + width],
        invalidate: () => {},
        handleInput: (data) => {
          if (data === "a") {
            doneA("alpha");
            return true;
          }
          return false;
        },
      }),
      { overlay: true, overlayOptions: { width: 20 } },
    );
    const second = ctx.ui.custom(
      (_tui, _theme, _kb, doneB) => ({
        render: (width) => ["SECOND " + width],
        invalidate: () => {},
        handleInput: (data) => {
          if (data === "b") {
            doneB("beta");
            return true;
          }
          return false;
        },
      }),
      { overlay: true, overlayOptions: { width: 30 } },
    );
    const results = await Promise.all([first, second]);
    pi.appendEntry("stack", { a: results[0] ?? null, b: results[1] ?? null });
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        let emitter = std::thread::spawn(move || {
            runner
                .lock()
                .expect("runner")
                .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
                .expect("emit");
        });
        for _ in 0..200 {
            mode.pump();
            if mode.extension_overlays.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        emitter.join().expect("emit thread");
        assert_eq!(mode.extension_overlays.len(), 2, "both overlays mounted");
        // Reset (the /reload path): both overlays go, newest first.
        mode.reset_extension_ui_dialogs();
        assert!(mode.extension_overlays.is_empty(), "both removed");
        assert_eq!(mode.tui.overlay_count(), 0);
        // The promises stay pending: no continuation ran, so no entry.
        let entries = mode
            .extension_runner
            .as_ref()
            .expect("runner")
            .lock()
            .expect("runner")
            .core()
            .lock()
            .expect("core")
            .entries
            .clone();
        assert!(
            !entries
                .iter()
                .any(|(custom_type, _)| custom_type == "stack"),
            "the custom promises never resolved: {entries:?}"
        );
        mode.shutdown().expect("shutdown");
    }

    /// `ctx.ui.custom` mounts the factory's adopted component in the
    /// editor slot; its render/input cross through the runner and `done()`
    /// resolves the extension's promise (`showExtensionCustom`,
    /// `interactive-mode.ts:3074-3163`).
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_custom_mounts_and_done_resolves() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const value = await ctx.ui.custom((_tui, _theme, _kb, done) => ({
      render: (width) => ["CUSTOM BODY " + width],
      invalidate: () => {},
      handleInput: (data) => {
        if (data === "\r") {
          done("picked-value");
          return true;
        }
        return false;
      },
    }));
    pi.appendEntry("picked", { value: value ?? null });
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        let emitter = std::thread::spawn(move || {
            runner
                .lock()
                .expect("runner")
                .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
                .expect("emit");
        });
        for _ in 0..200 {
            mode.pump();
            if !mode.extension_ui_dialogs.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let dialog = mode.extension_ui_dialogs.last().expect("custom mounted");
        assert_eq!(dialog.kind, ExtensionUiKind::Custom);
        assert!(mode.suspended_editor.is_some(), "the editor is parked");
        // The wrapper renders through the runner crossings.
        let component = mode
            .downcast_tree::<ExtensionCustomComponent>(HostSlot::Editor)
            .expect("the custom sits in the editor slot");
        assert_eq!(component.render(100), vec!["CUSTOM BODY 100".to_string()],);
        // Enter reaches it through the focused slot; done() closes.
        mode.feed_stdin_bytes(b"\r");
        mode.pump();
        emitter.join().expect("emit thread");
        let entries = mode
            .extension_runner
            .as_ref()
            .expect("runner")
            .lock()
            .expect("runner")
            .core()
            .lock()
            .expect("core")
            .entries
            .clone();
        let picked = entries
            .iter()
            .find(|(custom_type, _)| custom_type == "picked")
            .map(|(_, data)| data.clone());
        assert_eq!(picked, Some(serde_json::json!({ "value": "picked-value" })));
        assert!(mode.extension_ui_dialogs.is_empty(), "the custom closed");
        assert!(mode.suspended_editor.is_none(), "editor restored");
        mode.shutdown().expect("shutdown");
    }

    /// Two `ctx.ui.custom` calls stack: the second mounts over the first
    /// (which parks in its entry), and closing it resurfaces the first
    /// (upstream's `extensionDialogs` stack,
    /// `interactive-mode.ts:2833-2856`).
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_custom_stack_push_and_pop() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const first = ctx.ui.custom((_tui, _theme, _kb, doneA) => ({
      render: (width) => ["ALPHA " + width],
      invalidate: () => {},
      handleInput: (data) => {
        if (data === "a") {
          doneA("alpha");
          return true;
        }
        return false;
      },
    }));
    const second = ctx.ui.custom((_tui, _theme, _kb, doneB) => ({
      render: (width) => ["BETA " + width],
      invalidate: () => {},
      handleInput: (data) => {
        if (data === "b") {
          doneB("beta");
          return true;
        }
        return false;
      },
    }));
    const results = await Promise.all([first, second]);
    pi.appendEntry("stack", { a: results[0] ?? null, b: results[1] ?? null });
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        let emitter = std::thread::spawn(move || {
            runner
                .lock()
                .expect("runner")
                .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
                .expect("emit");
        });
        for _ in 0..200 {
            mode.pump();
            if mode.extension_ui_dialogs.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(mode.extension_ui_dialogs.len(), 2, "both customs mounted");
        assert!(
            mode.extension_ui_dialogs[0].component.is_some(),
            "the first custom is parked under the second"
        );
        assert!(
            mode.extension_ui_dialogs[1].component.is_none(),
            "the second custom is visible"
        );
        // BETA sits in the editor slot.
        let beta = mode
            .downcast_tree::<ExtensionCustomComponent>(HostSlot::Editor)
            .expect("the top custom sits in the editor slot");
        assert_eq!(beta.render(100), vec!["BETA 100".to_string()]);
        // "b" closes it; ALPHA resurfaces.
        mode.feed_stdin_bytes(b"b");
        mode.pump();
        assert_eq!(mode.extension_ui_dialogs.len(), 1, "BETA closed");
        let alpha = mode
            .downcast_tree::<ExtensionCustomComponent>(HostSlot::Editor)
            .expect("the first custom resurfaced");
        assert_eq!(alpha.render(100), vec!["ALPHA 100".to_string()]);
        mode.feed_stdin_bytes(b"a");
        mode.pump();
        emitter.join().expect("emit thread");
        let entries = mode
            .extension_runner
            .as_ref()
            .expect("runner")
            .lock()
            .expect("runner")
            .core()
            .lock()
            .expect("core")
            .entries
            .clone();
        let stack = entries
            .iter()
            .find(|(custom_type, _)| custom_type == "stack")
            .map(|(_, data)| data.clone());
        assert_eq!(
            stack,
            Some(serde_json::json!({ "a": "alpha", "b": "beta" }))
        );
        assert!(mode.extension_ui_dialogs.is_empty(), "both closed");
        assert!(mode.suspended_editor.is_none(), "editor restored");
        mode.shutdown().expect("shutdown");
    }

    /// A session switch leaves pending dialogs exactly as they are —
    /// upstream runs `resetExtensionUI` at init (`interactive-mode.ts:643`)
    /// and on `/reload` (`:6628`) only, and the extensions themselves
    /// persist across the switch. The still-mounted dialog stays
    /// interactive and resolves after the switch. A custom dialog rides
    /// here because its answer holds no runner mutex (a mailbox dialog
    /// opened by the emit blocks its emitter thread inline; the mode-side
    /// survival behavior is the same stack code for every kind).
    #[cfg(feature = "ext-js")]
    #[test]
    fn extension_ui_session_switch_leaves_dialogs_pending() {
        const EXT: &str = r#"
export default function (pi) {
  pi.on("session_start", async (_event, ctx) => {
    const value = await ctx.ui.custom((_tui, _theme, _kb, done) => ({
      render: (width) => ["KEEP ME " + width],
      invalidate: () => {},
      handleInput: (data) => {
        if (data === "z") {
          done("kept-value");
          return true;
        }
        return false;
      },
    }));
    pi.appendEntry("kept", { value: value ?? null });
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture, _bridge) = fixture_with_extension_ui(dir.path(), EXT);
        let runner = mode.extension_runner.clone().expect("runner");
        let emitter = std::thread::spawn(move || {
            runner
                .lock()
                .expect("runner")
                .emit(&serde_json::json!({ "type": "session_start", "reason": "startup" }))
                .expect("emit");
        });
        for _ in 0..200 {
            mode.pump();
            if !mode.extension_ui_dialogs.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        emitter.join().expect("emit thread");
        assert_eq!(mode.extension_ui_dialogs.len(), 1, "the custom mounted");
        // Switch to a fresh session while the dialog is open.
        let other = SessionManager::create(dir.path(), None, &NewSessionOptions::default())
            .expect("session");
        let other_path = other.session_file().expect("session file").to_path_buf();
        drop(other);
        mode.switch_session(&other_path);
        // Upstream-faithful: the dialog survives, still mounted and focused.
        assert_eq!(
            mode.extension_ui_dialogs.len(),
            1,
            "the switch does not dispose the dialog"
        );
        assert_eq!(mode.extension_ui_dialogs[0].kind, ExtensionUiKind::Custom);
        assert!(mode.suspended_editor.is_some(), "still parked");
        // And it still answers.
        mode.feed_stdin_bytes(b"z");
        mode.pump();
        let entries = mode
            .extension_runner
            .as_ref()
            .expect("runner")
            .lock()
            .expect("runner")
            .core()
            .lock()
            .expect("core")
            .entries
            .clone();
        let kept = entries
            .iter()
            .find(|(custom_type, _)| custom_type == "kept")
            .map(|(_, data)| data.clone());
        assert_eq!(kept, Some(serde_json::json!({ "value": "kept-value" })));
        assert!(
            mode.extension_ui_dialogs.is_empty(),
            "answered after switch"
        );
        assert!(mode.suspended_editor.is_none(), "editor restored");
        mode.shutdown().expect("shutdown");
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
        // `defaultWorkingMessage` (`interactive-mode.ts:505`): "Working".
        assert!(plain.contains("Working"), "{plain}");
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

    /// Streaming partials refresh an open tool row's args
    /// (`interactive-mode.ts:3674-3678`): the call line moves off the
    /// empty-command placeholder once the arguments arrive.
    #[test]
    fn streaming_tool_args_refresh_the_call_line() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        let call = |arguments: serde_json::Value| {
            serde_json::from_value::<pi_ai::types::ToolCall>(serde_json::json!({
                "id": "call-1", "name": "bash", "arguments": arguments
            }))
            .expect("tool call")
        };
        let partial = scripted_assistant(
            &[pi_ai::types::AssistantContent::ToolCall(call(
                serde_json::json!({
                    "command": ""
                }),
            ))],
            StopReason::Pending,
        );
        mode.start_assistant_stream();
        mode.update_assistant_stream(&partial);
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("$ ..."), "{plain}");
        let full = scripted_assistant(
            &[pi_ai::types::AssistantContent::ToolCall(call(
                serde_json::json!({ "command": "echo hi" }),
            ))],
            StopReason::ToolUse,
        );
        mode.update_assistant_stream(&full);
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("$ echo hi"), "{plain}");
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
        let markdown = hotkeys_markdown(&[]);
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

    /// The Extensions section (`interactive-mode.ts:7092-7106`): one row per
    /// resolved shortcut, the key capitalized, the description defaulting
    /// to the extension path.
    #[test]
    fn hotkeys_markdown_lists_extension_shortcuts() {
        let rows = vec![
            ("ctrl+shift+7".to_string(), "Toggle plan mode".to_string()),
            ("ctrl+q".to_string(), String::new()),
        ];
        let markdown = hotkeys_markdown(&rows);
        let plain = pi_tui::ansi::strip_terminal_sequences(&markdown);
        assert!(plain.contains("**Extensions**"), "{plain}");
        assert!(
            plain.contains("| `Ctrl+Shift+7` | Toggle plan mode |"),
            "{plain}"
        );
        // An empty description falls back to the extension path.
        assert!(plain.contains("| `Ctrl+Q` | "), "{plain}");
    }

    /// `custom-editor.ts:89-92`: the editor checks extension shortcuts
    /// before every built-in key — the matching chord is consumed and
    /// queued, a non-matching chord reaches the editor untouched.
    #[cfg(feature = "ext-js")] // the event variant is feature-gated
    #[test]
    fn extension_shortcuts_match_before_builtin_keys() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let shortcuts = Arc::new(Mutex::new(vec!["ctrl+q".to_string()]));
        let mut editor = ChatEditor::new(
            Arc::clone(&events),
            Arc::new(Mutex::new(None::<StatusIndicator>)),
            Arc::clone(&shortcuts),
        );
        // Ctrl+Q is the raw control byte 0x11; nothing built-in binds it,
        // the extension takes it.
        editor.handle_input("\x11");
        assert_eq!(
            events.lock().expect("events").clone(),
            vec![EditorEventOut::ExtensionShortcut("ctrl+q".to_string())]
        );
        // Plain text falls through to the editor.
        editor.handle_input("x");
        let drained = events.lock().expect("events").clone();
        assert_eq!(drained.len(), 1);
    }

    // ---- E3 slice 3: custom entry renderers ----

    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    fn custom_entry_record(custom_type: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "custom",
            "id": "e1",
            "timestamp": "2026-01-01T00:00:00Z",
            "customType": custom_type,
            "data": { "k": 7 }
        })
    }

    /// `entry_appended` renders the custom row through the extension
    /// renderer (`interactive-mode.ts:3569-3576`): the entry record and the
    /// theme cross into the JS component, and the row lands in the chat.
    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    #[test]
    fn entry_appended_renders_the_extension_custom_row() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", (entry, options, theme) => ({
    render: (width) => [theme.fg("error", `note:${entry.data.k}:${options.expanded}`)],
    invalidate: () => {},
  }));
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture_with_extension(dir.path(), EXT);
        let before = mode.chat_len();

        mode.handle_agent_event(&AgentEvent::EntryAppended {
            entry: custom_entry_record("note"),
        });
        assert_eq!(mode.chat_len(), before + 1);
        let chat = mode.chat_mut();
        let child = chat.get(before).expect("custom row");
        let lines = strip_terminal_sequences(&child.render(80).join("\n"));
        // The theme.fg("error", …) call crossed through the port theme, so
        // the styled text strips back to the plain content.
        assert!(lines.contains("note:7:false"), "{lines}");

        // A type with no renderer appends nothing (`:3988-3990`).
        mode.handle_agent_event(&AgentEvent::EntryAppended {
            entry: custom_entry_record("unknown-type"),
        });
        assert_eq!(mode.chat_len(), before + 1);
        mode.shutdown().expect("shutdown");
    }

    /// A renderer throw becomes the host's error box
    /// (`custom-entry.ts:44-50`, `interactive-mode.ts:3991-3995`).
    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    #[test]
    fn custom_entry_renderer_failure_renders_the_error_box() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerEntryRenderer("note", () => {
    throw new Error("boom-entry");
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture_with_extension(dir.path(), EXT);
        let before = mode.chat_len();

        mode.handle_agent_event(&AgentEvent::EntryAppended {
            entry: custom_entry_record("note"),
        });
        assert_eq!(mode.chat_len(), before + 1);
        let chat = mode.chat_mut();
        let child = chat.get(before).expect("error box row");
        let lines = strip_terminal_sequences(&child.render(80).join("\n"));
        assert!(
            lines.contains("[note] renderer failed: boom-entry"),
            "{lines}"
        );
        mode.shutdown().expect("shutdown");
    }

    /// Replayed custom entries without an extension runner are skipped
    /// (`renderSessionEntries` keeps them, but `addCustomEntryToChat`
    /// returns early — `interactive-mode.ts:3988-3990`).
    #[test]
    fn replayed_custom_entry_without_runner_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        let before = mode.chat_len();
        let custom = crate::session::records::CustomEntry {
            custom_type: "note".to_string(),
            data: Some(serde_json::json!({ "k": 7 })),
            base: crate::session::records::EntryBase {
                id: "e1".to_string(),
                parent_id: None,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            },
            unknown: serde_json::Map::default(),
        };
        mode.render_session_entries(&[SessionEntry::Custom(custom)], false);
        assert_eq!(mode.chat_len(), before);
        mode.shutdown().expect("shutdown");
    }

    // ---- E3 slice 4: custom message renderers ----

    /// The `custom_message` record `entry_appended` carries —
    /// `createCustomMessage` builds the live message from it
    /// (`interactive-mode.ts:3577-3587`).
    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    fn custom_message_record(custom_type: &str, display: bool) -> serde_json::Value {
        serde_json::json!({
            "type": "custom_message",
            "id": "m1",
            "timestamp": "2026-01-01T00:00:00Z",
            "customType": custom_type,
            "content": "hello body",
            "display": display,
            "details": { "k": 7 }
        })
    }

    /// `entry_appended` renders a displayed custom message through the
    /// extension renderer (`interactive-mode.ts:3577-3587` →
    /// `addMessageToChat` case "custom", `:4032-4048`); `display:false`
    /// renders nothing.
    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    #[test]
    fn custom_message_appended_renders_the_extension_row() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("note", (message, options, theme) => ({
    render: (width) => [theme.fg("error", `msg:${message.customType}:${options.outputPad}:${options.expanded}`)],
    invalidate: () => {},
  }));
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture_with_extension(dir.path(), EXT);
        let before = mode.chat_len();

        mode.handle_agent_event(&AgentEvent::EntryAppended {
            entry: custom_message_record("note", true),
        });
        assert_eq!(mode.chat_len(), before + 1);
        let chat = mode.chat_mut();
        let child = chat.get(before).expect("custom message row");
        let lines = strip_terminal_sequences(&child.render(80).join("\n"));
        // The record crossed (customType) with the output-pad option; the
        // theme.fg("error", …) call crossed through the port theme, so the
        // styled text strips back to the plain content.
        assert!(lines.contains("msg:note:1:false"), "{lines}");

        // `display:false` appends nothing (`:4033`).
        mode.handle_agent_event(&AgentEvent::EntryAppended {
            entry: custom_message_record("note", false),
        });
        assert_eq!(mode.chat_len(), before + 1);
        mode.shutdown().expect("shutdown");
    }

    /// A message renderer throw falls through to the default box silently —
    /// unlike the custom ENTRY error box (`custom-message.ts:93-95`) — and
    /// a type with no renderer takes the same default path (`:4034`).
    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    #[test]
    fn custom_message_renderer_failure_takes_the_default_box() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("note", () => {
    throw new Error("boom-message");
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture_with_extension(dir.path(), EXT);
        let before = mode.chat_len();

        mode.handle_agent_event(&AgentEvent::EntryAppended {
            entry: custom_message_record("note", true),
        });
        assert_eq!(mode.chat_len(), before + 1);
        let chat = mode.chat_mut();
        let child = chat.get(before).expect("default box row");
        let lines = strip_terminal_sequences(&child.render(80).join("\n"));
        // The default box: the bold [note] label and the content text; the
        // throw is swallowed (no "renderer failed" row for messages).
        assert!(lines.contains("[note]"), "{lines}");
        assert!(lines.contains("hello body"), "{lines}");
        assert!(!lines.contains("renderer failed"), "{lines}");

        // No renderer for the type: the same default box.
        mode.handle_agent_event(&AgentEvent::EntryAppended {
            entry: custom_message_record("other", true),
        });
        assert_eq!(mode.chat_len(), before + 2);
        let chat = mode.chat_mut();
        let child = chat.get(before + 1).expect("second row");
        let lines = strip_terminal_sequences(&child.render(80).join("\n"));
        assert!(lines.contains("[other]"), "{lines}");
        assert!(lines.contains("hello body"), "{lines}");
        mode.shutdown().expect("shutdown");
    }

    /// Replayed custom messages go through the generic message path with
    /// the persisted meta (`session-manager.ts:453-456`,
    /// `interactive-mode.ts:4268-4282`): the default box renders, and the
    /// `custom`-surface markdown transformers see the entry id/timestamp.
    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    #[test]
    fn replayed_custom_message_renders_with_transformer_meta() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerMarkdownTransformer(
    (markdown, context) => markdown + ` @${context.messageId}|${context.timestamp}`,
    { messageTypes: ["custom"] },
  );
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture_with_extension(dir.path(), EXT);
        let before = mode.chat_len();
        let custom = crate::session::records::CustomMessageEntry {
            custom_type: "note".to_string(),
            content: pi_ai::types::UserContent::Text("hello body".to_string()),
            display: true,
            details: None,
            base: crate::session::records::EntryBase {
                id: "m1".to_string(),
                parent_id: None,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            },
            unknown: serde_json::Map::default(),
        };
        mode.render_session_entries(&[SessionEntry::CustomMessage(custom)], false);
        assert_eq!(mode.chat_len(), before + 1);
        let chat = mode.chat_mut();
        let child = chat.get(before).expect("default box row");
        let lines = strip_terminal_sequences(&child.render(80).join("\n"));
        assert!(lines.contains("[note]"), "{lines}");
        assert!(lines.contains("hello body"), "{lines}");
        // The persisted meta crossed to the transformer (live rows carry none).
        assert!(lines.contains("@m1|2026-01-01T00:00:00Z"), "{lines}");
        mode.shutdown().expect("shutdown");
    }

    // ---- E3 slice 5: tool renderers ----

    /// The resolver chain resolves at row construction
    /// (`getRegisteredToolDefinition`, `interactive-mode.ts:2356-2360`): a
    /// resolver-supplied `renderCall` component replaces the built-in call
    /// line, with the context flags crossing.
    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    #[test]
    fn extension_tool_renderer_draws_the_call_line() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerToolRenderer((toolName, next) => {
    if (toolName === "read") {
      return {
        renderCall: (args, theme, context) => ({
          render: () => ["ext-call:" + args.path + ":" + context.outputPad + ":" + context.cwd],
          invalidate: () => {},
        }),
      };
    }
    return next();
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture_with_extension(dir.path(), EXT);
        let js = mode.resolve_tool_renderers("read");
        assert!(js.is_some(), "the resolver supplied renderers for read");
        let component =
            crate::modes::interactive::components::tool_execution::ToolExecutionComponent::new(
                "read",
                "call-1",
                serde_json::json!({ "path": "/tmp/example.txt" }),
                1,
                "/tmp",
                crate::tools::renderers::renderers_for("read"),
                js,
            );
        let lines = strip_terminal_sequences(&component.render(60).join("\n"));
        assert!(
            lines.contains("ext-call:/tmp/example.txt:1:/tmp"),
            "{lines}"
        );
        mode.shutdown().expect("shutdown");
    }

    /// A custom `renderCall` throw takes the generic line, like upstream's
    /// catch (`tool-execution.ts:309-313`) — the row still renders.
    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    #[test]
    fn extension_tool_renderer_throw_takes_the_generic_line() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerToolRenderer((_toolName, _next) => ({
    renderCall: () => {
      throw new Error("boom-call");
    },
  }));
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture_with_extension(dir.path(), EXT);
        let js = mode.resolve_tool_renderers("read");
        assert!(js.is_some());
        let component =
            crate::modes::interactive::components::tool_execution::ToolExecutionComponent::new(
                "read",
                "call-1",
                serde_json::json!({ "path": "/tmp/example.txt" }),
                1,
                "/tmp",
                crate::tools::renderers::renderers_for("read"),
                js,
            );
        let lines = strip_terminal_sequences(&component.render(60).join("\n"));
        // The generic fallback: the collapsed `formatToolCallWithArgs` line.
        assert!(lines.contains("read"), "{lines}");
        assert!(lines.contains(r#"path="/tmp/example.txt""#), "{lines}");
        mode.shutdown().expect("shutdown");
    }

    /// A resolver can supply renderers for tools with no Rust built-in
    /// (`withBuiltInRenderers` base is undefined there —
    /// `renderers/index.ts:51-63`); without a resolver the row takes the
    /// generic path.
    #[cfg(feature = "ext-js")] // the renderer registry is feature-gated
    #[test]
    fn unknown_tool_with_and_without_a_resolver() {
        const EXT: &str = r#"
export default function (pi) {
  pi.registerToolRenderer((toolName, next) => {
    if (toolName === "synth") {
      return {
        renderCall: () => ({ render: () => ["synth-row"], invalidate: () => {} }),
      };
    }
    return next();
  });
}
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, _capture) = fixture_with_extension(dir.path(), EXT);
        let js = mode.resolve_tool_renderers("synth");
        assert!(js.is_some(), "the resolver supplied unknown-tool renderers");
        let component =
            crate::modes::interactive::components::tool_execution::ToolExecutionComponent::new(
                "synth",
                "call-1",
                serde_json::json!({ "k": 1 }),
                1,
                "/tmp",
                crate::tools::renderers::renderers_for("synth"),
                js,
            );
        let lines = strip_terminal_sequences(&component.render(60).join("\n"));
        assert!(lines.contains("synth-row"), "{lines}");

        // An unknown tool with no resolver: the generic row.
        let js = mode.resolve_tool_renderers("other-synth");
        assert!(js.is_none());
        let component =
            crate::modes::interactive::components::tool_execution::ToolExecutionComponent::new(
                "other-synth",
                "call-2",
                serde_json::json!({ "k": 1 }),
                1,
                "/tmp",
                crate::tools::renderers::renderers_for("other-synth"),
                js,
            );
        let lines = strip_terminal_sequences(&component.render(60).join("\n"));
        assert!(lines.contains("other-synth"), "{lines}");
        assert!(lines.contains("k=1"), "{lines}");
        mode.shutdown().expect("shutdown");
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
        // Builtin providers render their upstream display name; unknown ones
        // fall back to the id (`oauth-selector.ts:30-49`).
        assert!(frame.contains("Groq [API key]"), "{frame}");
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
        assert!(frame.contains("Removed stored API key for Groq"), "{frame}");
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
        assert!(frame.contains("Saved API key for Groq"), "{frame}");
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
        // The Radius option rides last (`interactive-mode.ts:6090-6093`),
        // labeled with the provider's display name (`radius.ts:23`).
        assert!(frame.contains("Sign in with Radius"), "{frame}");

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

    /// The extension-oauth end-to-end (`loginProvider`'s `"oauth"` arm
    /// through the engine bridge, slice 6j): the registration's `login`
    /// notifies, asks, and settles; the pump mounts the ask, the answer
    /// crosses back token-verified, and the credential lands in auth.json
    /// with the `type: "oauth"` shape the ladder resolves through
    /// `getApiKey`.
    #[cfg(feature = "ext-js")]
    #[test]
    fn login_extension_oauth_drives_the_engine_flow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut mode, capture) = fixture(dir.path(), "/nonexistent");
        mode.tui.start().expect("tui start");

        let source = r#"
export default function (pi) {
  pi.registerProvider("acme", {
    api: "acme-api",
    oauth: {
      name: "Acme",
      login: async (cb) => {
        cb.onProgress("connecting");
        const code = await cb.onPrompt({ message: "paste the acme code", placeholder: "code" });
        return { access: `tok-${code}`, refresh: "r", expires: 1 };
      },
    },
  });
}
"#;
        let mut runner = pi_ext_js::ExtensionRunner::new(pi_ext_js::HostCore {
            cwd: "/tmp".to_string(),
            mode: "print".to_string(),
            settings: serde_json::json!({}),
            ..Default::default()
        })
        .expect("runner");
        runner
            .load_extension("<test-0>", Some(source))
            .expect("load");
        runner.bind().expect("bind");
        mode.extension_runner = Some(Arc::new(Mutex::new(runner)));

        // The selector listing rides the composed snapshot (covered by the
        // slice 6c tests); the flow test drives `startProviderLogin`
        // directly with the row that listing would produce.
        let provider = AuthSelectorProvider {
            id: "acme".to_string(),
            name: "acme".to_string(),
            auth_type: "oauth",
            subscription: false,
            method_name: Some("Acme".to_string()),
            login_label: None,
            status: None,
        };
        mode.start_provider_login(&provider, None);
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("connecting"), "progress shown: {frame}");
        let flow = mode.extension_login_flow.as_ref().expect("flow started");
        assert!(flow.pending_prompt.is_some(), "ask suspended the flow");
        assert!(mode.extension_login_prompt.is_some(), "prompt mounted");

        mode.extension_login_prompt
            .as_ref()
            .expect("prompt")
            .lock()
            .expect("prompt events")
            .push(LoginPromptEvent::Submit("code-1".to_string()));
        mode.pump();
        mode.tui.render_now(false);
        let frame = pi_tui::ansi::strip_terminal_sequences(&capture.output());
        assert!(frame.contains("Logged in to acme"), "{frame}");
        assert!(frame.contains("Credentials saved to"), "{frame}");
        assert!(mode.extension_login_flow.is_none(), "flow settled");
        assert!(mode.extension_login_prompt.is_none(), "prompt closed");
        assert!(mode.suspended_editor.is_none(), "editor restored");
        let stored = std::fs::read_to_string(dir.path().join("auth.json")).expect("auth.json");
        assert!(stored.contains(r#""acme""#), "{stored}");
        assert!(stored.contains("tok-code-1"), "{stored}");
        assert!(stored.contains(r#""oauth""#), "{stored}");

        mode.shutdown().expect("shutdown");
    }

    // --------------------------------------------------------------
    // /fork and /clone (mirrors `interactive-mode-clone-command.test.ts`
    // and the fork dialog flow in `interactive-mode.ts:5692-5728`)
    // --------------------------------------------------------------

    /// Two appended messages for the clone fixtures; returns the entry ids.
    fn seed_two_messages(mode: &mut InteractiveMode) -> (String, String) {
        let user = mode
            .session
            .append_message(AgentMessage::User(pi_ai::types::UserMessage {
                content: pi_ai::types::UserContent::Text("first message".to_string()),
                timestamp: 1,
            }))
            .expect("append user");
        let assistant = mode
            .session
            .append_message(test_assistant_message("the reply"))
            .expect("append assistant");
        (user.id().to_string(), assistant.id().to_string())
    }

    /// A minimal assistant message (the wire fields the renderers read),
    /// like [`scripted_assistant`] without the stream.
    fn test_assistant_message(text: &str) -> AgentMessage {
        serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
            "api": "test",
            "provider": "test",
            "model": "test-model",
            "usage": {
                "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                "totalTokens": 2,
                "cost": { "input": 0, "output": 0, "cacheRead": 0,
                          "cacheWrite": 0, "total": 0 }
            },
            "stopReason": "stop",
            "timestamp": 1
        }))
        .expect("assistant message")
    }

    /// `/clone` on an empty session: "Nothing to clone yet", no fork
    /// (`interactive-mode-clone-command.test.ts:51-71`).
    #[test]
    fn clone_without_entries_reports_nothing_to_clone() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");

        mode.submit("/clone");
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Nothing to clone yet"), "{plain}");
        assert!(!plain.contains("Cloned to new session"), "{plain}");
    }

    /// `/clone` at the leaf duplicates the path into a fresh session
    /// (`interactive-mode-clone-command.test.ts:23-49`: fork called at the
    /// leaf with `position: "at"`, "Cloned to new session", no error).
    #[test]
    fn clone_copies_the_session_at_the_current_position() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        let (user_id, assistant_id) = seed_two_messages(&mut mode);
        let old_session_id = mode.session.get_session_id().to_string();

        mode.submit("/clone");
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Cloned to new session"), "{plain}");
        assert!(!plain.contains("Invalid entry ID"), "{plain}");
        // The store swapped to a fresh session holding the branched path
        // (`createBranchedSession`): same entries, new id, leaf kept.
        assert_ne!(mode.session.get_session_id(), old_session_id);
        assert_eq!(mode.session.get_entry_count(), 2);
        assert_eq!(mode.session.get_leaf_id(), Some(assistant_id.as_str()));
        let ids: Vec<String> = mode
            .session
            .get_entries()
            .iter()
            .filter_map(|entry| {
                if entry.id().is_empty() {
                    None
                } else {
                    Some(entry.id().to_string())
                }
            })
            .collect();
        assert_eq!(ids, vec![user_id, assistant_id]);
    }

    /// `/fork` with no user messages: the status line, no dialog
    /// (`showUserMessageSelector`'s empty guard, `interactive-mode.ts:5695-5698`).
    #[test]
    fn fork_without_messages_shows_status() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");

        mode.submit("/fork");
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("No messages to fork from"), "{plain}");
        assert!(mode.suspended_editor.is_none(), "no dialog mounted");
    }

    /// `/fork` mounts the dialog on the newest message, and selecting one
    /// forks to a session ending at its parent with the text back in the
    /// editor (`showUserMessageSelector` + the fork callbacks,
    /// `interactive-mode.ts:5700-5725`).
    #[test]
    fn fork_dialog_forks_from_the_selected_user_message() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        let (user_id, _assistant_id) = seed_two_messages(&mut mode);

        mode.submit("/fork");
        assert!(
            mode.suspended_editor.is_some(),
            "the fork dialog occupies the editor slot"
        );
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Fork from Message"), "{plain}");
        assert!(plain.contains("first message"), "{plain}");
        // The initial selection is the most recent message — the assistant
        // reply is not forkable, so the user message is last.
        assert!(plain.contains("Message 1 of 1"), "{plain}");

        mode.user_message_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events")
            .push(UserMessageSelectorEvent::Selected(user_id.clone()));
        mode.pump();
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Forked to new session"), "{plain}");
        assert!(mode.suspended_editor.is_none(), "editor restored");
        // The fork ends at the user message's parent — the root — so the new
        // session starts empty, and the message text returns for editing.
        assert_eq!(mode.session.get_entry_count(), 0);
        assert!(mode.session.get_leaf_id().is_none());
        assert_eq!(mode.editor_mut().editor.get_text(), "first message");
    }

    /// The fork dialog's cancel restores the editor without forking
    /// (`onCancel` → `done()` + `requestRender`, `interactive-mode.ts:5720-5723`).
    #[test]
    fn fork_dialog_cancel_restores_the_editor() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        let (user_id, assistant_id) = seed_two_messages(&mut mode);
        let old_session_id = mode.session.get_session_id().to_string();

        mode.submit("/fork");
        assert!(mode.suspended_editor.is_some(), "dialog mounted");
        mode.tui.render_now(false);
        mode.user_message_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events")
            .push(UserMessageSelectorEvent::Cancel);
        mode.pump();
        mode.tui.render_now(false);

        assert!(mode.suspended_editor.is_none(), "editor restored");
        assert!(mode.user_message_selector_events.is_none());
        assert_eq!(mode.session.get_session_id(), old_session_id);
        assert_eq!(mode.session.get_entry_count(), 2);
        assert_eq!(mode.session.get_leaf_id(), Some(assistant_id.as_str()));
        assert!(
            mode.session.get_entry(&user_id).is_some(),
            "the original session is untouched"
        );
        let plain = strip_terminal_sequences(&capture.output());
        assert!(!plain.contains("Forked to new session"), "{plain}");
    }

    // --------------------------------------------------------------
    // /tree (mirrors `interactive-mode-tree-navigation.test.ts` and the
    // `showTreeSelector` flow in `interactive-mode.ts:5751-5898`)
    // --------------------------------------------------------------

    /// `/tree` on an empty session: the status line, no dialog
    /// (`showTreeSelector`'s empty guard, `interactive-mode.ts:5756-5759`).
    #[test]
    fn tree_selector_on_an_empty_session_shows_status() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");

        mode.submit("/tree");
        mode.tui.render_now(false);
        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("No entries in session"), "{plain}");
        assert!(mode.suspended_editor.is_none(), "no dialog mounted");
    }

    /// Confirming the current leaf is a no-op with its own status
    /// (`interactive-mode.ts:5767-5772`).
    #[test]
    fn tree_selection_on_the_current_leaf_reports_already_there() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        let (_user_id, assistant_id) = seed_two_messages(&mut mode);

        mode.submit("/tree");
        assert!(mode.suspended_editor.is_some(), "the tree dialog mounts");
        // The leaf is the assistant reply (the last entry).
        mode.tree_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events")
            .push(TreeSelectorEvent::Selected(assistant_id));
        mode.pump();
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Already at this point"), "{plain}");
        assert!(mode.suspended_editor.is_none(), "editor restored");
        // The leaf never moved and no dialog chain started.
        assert!(mode.tree_navigation.is_none());
        assert_eq!(mode.session.get_entry_count(), 2);
    }

    /// The summarize prompt mounts after the selection, and choosing
    /// "No summary" navigates to the target (`interactive-mode.ts`
    /// :5783-5796, :5839-5863).
    #[test]
    fn tree_navigates_without_a_summary() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        let (user_id, _assistant_id) = seed_two_messages(&mut mode);

        mode.editor_events
            .lock()
            .expect("editor events")
            .push(EditorEventOut::Submit("/tree".to_string()));
        mode.pump();
        mode.tree_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events")
            .push(TreeSelectorEvent::Selected(user_id));
        mode.pump();

        // The "Summarize branch?" dialog occupies the editor slot.
        assert!(mode.suspended_editor.is_some(), "summary dialog mounts");
        mode.tree_summary_dialog_events
            .as_ref()
            .expect("summary dialog")
            .lock()
            .expect("summary dialog events")
            .push(ExtensionSelectorEvent::Selected("No summary".to_string()));
        mode.pump();
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Navigated to selected point"), "{plain}");
        assert!(mode.suspended_editor.is_none(), "editor restored");
        // The target user message sits at the root: the leaf resets and its
        // text returns for editing (`navigateTree`'s user-message branch,
        // `agent-session.ts:4269-4272`).
        assert!(mode.session.get_leaf_id().is_none());
        assert_eq!(mode.session.get_entry_count(), 2, "no summary entry");
        assert_eq!(mode.editor_mut().editor.get_text(), "first message");
        assert!(mode.tree_navigation.is_none(), "navigation settled");
    }

    /// A text-only scripted stream for the summarizer: the branch summary
    /// call must not carry tool calls (`generateBranchSummary` rejects them).
    struct SummaryStream;

    impl pi_agent::types::StreamFn for SummaryStream {
        fn stream(
            &self,
            _request: pi_agent::types::StreamRequest,
        ) -> futures::stream::BoxStream<'static, AssistantMessageEvent> {
            let partial = scripted_assistant(
                &[pi_ai::types::AssistantContent::Text(TextContent {
                    text: "## Goal\nbranch work summarized".to_string(),
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

    /// A branch summary runs on the worker and lands as a `branch_summary`
    /// entry attached at the navigation target (`agent-session.ts`
    /// :4230-4306).
    #[test]
    fn tree_summarize_attaches_a_branch_summary_at_the_target() {
        let dir = tempfile::tempdir().expect("dir");
        let stream: Arc<dyn pi_agent::types::StreamFn> = Arc::new(SummaryStream);
        let (mut mode, capture) = fixture_with_stream(dir.path(), stream);
        mode.tui.start().expect("tui start");
        let (user_id, _assistant_id) = seed_two_messages(&mut mode);

        mode.submit("/tree");
        mode.tree_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events")
            .push(TreeSelectorEvent::Selected(user_id));
        mode.pump();
        mode.tree_summary_dialog_events
            .as_ref()
            .expect("summary dialog")
            .lock()
            .expect("summary dialog events")
            .push(ExtensionSelectorEvent::Selected("Summarize".to_string()));
        mode.pump();

        // The worker starts; the summary status rides the editor border.
        assert!(mode.tree_navigation.is_some(), "navigation staged");
        for _ in 0..200 {
            mode.pump();
            mode.tui.render_now(false);
            if mode.tree_navigation.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Navigated to selected point"), "{plain}");
        // The summary attached at the target position (the root), and the
        // leaf moved onto the summary entry (`agent-session.ts:4287-4294`).
        assert_eq!(mode.session.get_entry_count(), 3);
        let summaries = mode
            .session
            .get_entries()
            .into_iter()
            .filter(|entry| entry.kind() == "branch_summary")
            .count();
        assert_eq!(summaries, 1);
        assert!(mode.session.get_leaf_id().is_some(), "leaf on the summary");
        assert_eq!(mode.editor_mut().editor.get_text(), "first message");
        assert!(mode.tree_navigation.is_none());
        assert!(mode.suspended_editor.is_none(), "editor restored");
    }

    /// Choosing a summary while busy preserves the active operation's UI and
    /// reports the busy error (regression #9178 / PR #9179,
    /// `interactive-mode-tree-navigation.test.ts:73-91`).
    #[test]
    fn tree_busy_guard_preserves_the_operation_ui() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        let (user_id, _assistant_id) = seed_two_messages(&mut mode);
        let original_leaf = mode.session.get_leaf_id().map(str::to_string);

        mode.submit("/tree");
        mode.tree_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events")
            .push(TreeSelectorEvent::Selected(user_id));
        mode.pump();
        // Compaction or another navigation can start while the dialog is
        // open (`ui.session.isCompacting = true`).
        mode.compaction_run = Some(CompactionRun {
            abort: Arc::new(pi_agent::types::AbortSignal::new()),
            events: std::sync::mpsc::channel().1,
            handle: None,
        });
        mode.tree_summary_dialog_events
            .as_ref()
            .expect("summary dialog")
            .lock()
            .expect("summary dialog events")
            .push(ExtensionSelectorEvent::Selected("No summary".to_string()));
        mode.pump();
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        // `showError` prepends `Error: ` (`interactive-mode.ts:4809-4813`);
        // at 100 columns the prefixed line wraps, so assert per line.
        assert!(
            plain.contains("Error: Wait for the current compaction or tree navigation to finish"),
            "{plain}"
        );
        assert!(plain.contains(" tree."), "{plain}");
        // The original leaf stands; no navigation started and the editor
        // came back.
        assert_eq!(
            mode.session.get_leaf_id().map(str::to_string),
            original_leaf
        );
        assert!(mode.tree_navigation.is_none());
        assert!(mode.suspended_editor.is_none(), "editor restored");
    }

    /// An active response aborts (and settles) before the navigation
    /// proceeds (`interactive-mode.ts:5811-5815`,
    /// `interactive-mode-tree-navigation.test.ts:110-124`).
    #[test]
    fn tree_navigation_aborts_the_active_response_first() {
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "content\n").expect("write");
        let (mut mode, capture) = fixture(dir.path(), &file.to_string_lossy());
        mode.tui.start().expect("tui start");
        let (user_id, _assistant_id) = seed_two_messages(&mut mode);

        // A streaming run to abort.
        mode.start_agent_run("hello there");
        assert!(mode.agent_run.is_some(), "run started");

        mode.submit("/tree");
        mode.tree_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events")
            .push(TreeSelectorEvent::Selected(user_id));
        mode.pump();
        mode.tree_summary_dialog_events
            .as_ref()
            .expect("summary dialog")
            .lock()
            .expect("summary dialog events")
            .push(ExtensionSelectorEvent::Selected("No summary".to_string()));
        mode.pump();
        mode.tui.render_now(false);

        let plain = strip_terminal_sequences(&capture.output());
        assert!(plain.contains("Navigated to selected point"), "{plain}");
        assert!(mode.agent_run.is_none(), "the run settled");
        assert!(mode.session.get_leaf_id().is_none(), "leaf reset to root");
    }

    /// A label change from the tree dialog persists through
    /// `appendLabelChange` (`interactive-mode.ts:5877-5880`).
    #[test]
    fn tree_label_change_persists() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut mode, _capture) = fixture(dir.path(), "unused.txt");
        mode.tui.start().expect("tui start");
        let (user_id, _assistant_id) = seed_two_messages(&mut mode);

        mode.submit("/tree");
        mode.tree_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events")
            .push(TreeSelectorEvent::LabelChange(
                user_id.clone(),
                Some("checkpoint".to_string()),
            ));
        mode.pump();
        mode.tui.render_now(false);

        assert_eq!(mode.session.get_label(&user_id), Some("checkpoint"));

        // A second change replaces it; an empty label clears it.
        mode.tree_selector_events
            .as_ref()
            .expect("selector events")
            .lock()
            .expect("selector events")
            .push(TreeSelectorEvent::LabelChange(user_id.clone(), None));
        mode.pump();
        assert_eq!(mode.session.get_label(&user_id), None);
    }
}
