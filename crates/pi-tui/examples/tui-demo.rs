//! TUI demo — port of upstream `test/chat-simple.ts` ("simple chat").
//!
//! Two modes:
//!
//! ```text
//! cargo run -p pi-tui --example tui-demo             # interactive (Ctrl+C exits)
//! cargo run -p pi-tui --example tui-demo -- --scripted
//! ```
//!
//! `--scripted` drives the same app against a [`CaptureTerminal`] with a
//! scripted keystroke sequence and prints the rendered screen after every
//! step — an offline end-to-end pass over the real render path (no tty).

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_tui::component::{Component, ComponentMutation};
use pi_tui::components::editor::{Editor, EditorOptions, EditorTheme};
use pi_tui::components::loader::Loader;
use pi_tui::components::markdown::{Markdown, MarkdownOptions, MarkdownTheme};
use pi_tui::components::text::Text;
use pi_tui::mouse::{MouseEvent, MouseEventResult};
use pi_tui::terminal::{CaptureTerminal, CrosstermTerminal, TerminalSize, key_to_terminal_data};
use pi_tui::tui::{FocusTarget, Tui, TuiStopOptions, plain_lines};

const RESPONSES: [&str; 8] = [
    "That's interesting! Tell me more.",
    "I see what you mean.",
    "Fascinating perspective!",
    "Could you elaborate on that?",
    "That makes sense to me.",
    "I hadn't thought of it that way.",
    "Great point!",
    "Thanks for sharing that.",
];

/// Publishes the nested [`Editor`]'s submissions to the host through a shared
/// queue — the tree cannot be downcast, so the host wraps instead.
struct EditorHost {
    editor: Editor,
    submitted: Arc<Mutex<Vec<String>>>,
}

impl EditorHost {
    fn set_disable_submit(&mut self, disabled: bool) {
        self.editor.disable_submit = disabled;
    }
}

impl Component for EditorHost {
    fn render(&self, width: usize) -> Vec<String> {
        self.editor.render(width)
    }

    fn handle_input(&mut self, data: &str) -> Option<ComponentMutation> {
        let mutation = Component::handle_input(&mut self.editor, data);
        while let Some(value) = self.editor.take_submitted() {
            self.submitted.lock().expect("submits").push(value);
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
}

/// The simple-chat app: welcome text, echo responses, and the editor as the
/// always-last child. `chat-simple.ts`.
struct ChatApp {
    tui: Tui,
    /// Children count before the first submit: conversation slots live
    /// between the welcome text and the editor.
    base_children: usize,
    submitted: Arc<Mutex<Vec<String>>>,
    responding: bool,
    response_index: usize,
}

impl ChatApp {
    fn new(tui: Tui) -> Self {
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let mut app = Self {
            tui,
            base_children: 0,
            submitted,
            responding: false,
            response_index: 0,
        };
        app.tui.add_child(Box::new(Text::new(
            "Welcome to Simple Chat!\n\nType your messages below. Press Ctrl+C to exit.",
            0,
            0,
        )));
        app.base_children = app.tui.children_len();
        let mut editor = Editor::new(EditorTheme::default(), EditorOptions::default());
        editor.set_placeholder(Some("Type a message…".to_string()));
        app.tui.add_child(Box::new(EditorHost {
            editor,
            submitted: Arc::clone(&app.submitted),
        }));
        app.tui
            .set_focus(Some(FocusTarget::Root(app.tui.children_len() - 1)));
        app
    }

    fn editor_host_mut(&mut self) -> Option<&mut EditorHost> {
        let index = self.tui.children_len().checked_sub(1)?;
        self.tui
            .child_mut(index)?
            .as_any_mut()?
            .downcast_mut::<EditorHost>()
    }

    /// Drain submissions recorded by the wrapped editor.
    fn take_submitted(&mut self) -> Vec<String> {
        std::mem::take(&mut self.submitted.lock().expect("submits"))
    }

    /// Feed one raw input chunk; `false` means exit (Ctrl+C).
    fn feed(&mut self, data: &str) -> bool {
        if data == "\x03" {
            return false;
        }
        self.tui.handle_input(data);
        for value in self.take_submitted() {
            self.submit(&value);
        }
        true
    }

    fn submit(&mut self, value: &str) {
        let trimmed = value.trim();
        if trimmed == "/clear" {
            while self.tui.children_len() > self.base_children + 1 {
                self.tui.remove_child(self.base_children);
            }
            // Removals shift indices: focus follows the editor to the end.
            self.tui
                .set_focus(Some(FocusTarget::Root(self.tui.children_len() - 1)));
            self.tui.request_render();
            return;
        }
        if trimmed == "/delete" {
            if self.tui.children_len() > self.base_children + 1 {
                let index = self.tui.children_len() - 2;
                self.tui.remove_child(index);
            }
            self.tui
                .set_focus(Some(FocusTarget::Root(self.tui.children_len() - 1)));
            self.tui.request_render();
            return;
        }
        if trimmed.is_empty() || self.responding {
            return;
        }
        self.responding = true;
        if let Some(host) = self.editor_host_mut() {
            host.set_disable_submit(true);
        }
        let user_message = Markdown::new(
            value,
            1,
            1,
            MarkdownTheme::default(),
            None,
            MarkdownOptions::default(),
        );
        self.insert_before_editor(Box::new(user_message));
        let loader = Loader::new(
            |s: &str| s.to_string(),
            |s: &str| s.to_string(),
            "Thinking...",
            None,
        );
        self.insert_before_editor(Box::new(loader));
        self.tui.request_render();
    }

    /// Swap the loader for the canned response (upstream's `setTimeout`).
    fn finish_response(&mut self) {
        if !self.responding {
            return;
        }
        self.responding = false;
        if let Some(host) = self.editor_host_mut() {
            host.set_disable_submit(false);
        }
        let index = self.tui.children_len() - 2;
        self.tui.remove_child(index);
        let response = RESPONSES[self.response_index % RESPONSES.len()];
        self.response_index += 1;
        let bot_message = Markdown::new(
            response,
            1,
            1,
            MarkdownTheme::default(),
            None,
            MarkdownOptions::default(),
        );
        self.insert_before_editor(Box::new(bot_message));
        self.tui.request_render();
    }

    fn insert_before_editor(&mut self, component: Box<dyn Component>) {
        // `add_child` appends; drop the editor, insert, re-add it.
        let index = self.tui.children_len() - 1;
        let editor = self.tui.remove_child(index);
        self.tui.add_child(component);
        self.tui.add_child(editor);
        self.tui
            .set_focus(Some(FocusTarget::Root(self.tui.children_len() - 1)));
    }

    fn render(&mut self) {
        self.tui.render_now(false);
    }

    /// The rendered screen with trailing whitespace and blank tail rows cut.
    fn screen(&self) -> String {
        let lines: Vec<String> = plain_lines(self.tui.last_lines())
            .iter()
            .map(|line| line.trim_end().to_string())
            .collect();
        let last = lines
            .iter()
            .rposition(|line| !line.is_empty())
            .map_or(0, |position| position + 1);
        lines[..last].join("\n")
    }
}

fn scripted() -> io::Result<()> {
    let terminal = CaptureTerminal::with_size(TerminalSize {
        columns: 72,
        rows: 20,
    });
    let mut app = ChatApp::new(Tui::new(Box::new(terminal)));
    app.tui.start()?;
    app.render();
    let mut step = 0usize;
    let show = |step: &mut usize, caption: &str, app: &ChatApp| {
        *step += 1;
        println!("── step {:2}: {} ──", *step, caption);
        println!("{}", app.screen());
        println!();
    };
    show(&mut step, "initial screen", &app);

    for ch in "hello".chars() {
        if !app.feed(&ch.to_string()) {
            break;
        }
    }
    app.render();
    show(&mut step, "typed \"hello\" into the editor", &app);

    app.feed("\r");
    app.render();
    show(&mut step, "submitted: user message + loader", &app);

    app.finish_response();
    app.render();
    show(&mut step, "response arrived (markdown)", &app);

    for ch in "/clear".chars() {
        if !app.feed(&ch.to_string()) {
            break;
        }
    }
    app.feed("\r");
    app.render();
    show(&mut step, "/clear wiped the conversation", &app);

    for ch in "round two".chars() {
        if !app.feed(&ch.to_string()) {
            break;
        }
    }
    app.feed("\r");
    app.finish_response();
    app.render();
    show(&mut step, "second round-trip", &app);

    app.tui.stop(TuiStopOptions::default())?;
    Ok(())
}

fn interactive() -> io::Result<()> {
    let mut app = ChatApp::new(Tui::new(Box::new(CrosstermTerminal::default())));
    app.tui.start()?;
    app.tui.request_render();
    let mut responding_since: Option<Instant> = None;
    loop {
        app.render();
        io::stdout().flush().ok();
        if let Some(started) = responding_since {
            if started.elapsed() >= Duration::from_secs(1) {
                app.finish_response();
                responding_since = None;
            }
        }
        if !crossterm::event::poll(Duration::from_millis(50))? {
            continue;
        }
        let crossterm::event::Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        if key.kind == crossterm::event::KeyEventKind::Release {
            continue;
        }
        let Some(data) = key_to_terminal_data(key) else {
            continue;
        };
        if !app.feed(&data) {
            app.tui.stop(TuiStopOptions::default())?;
            return Ok(());
        }
        if app.responding && responding_since.is_none() {
            responding_since = Some(Instant::now());
        }
    }
}

fn main() -> io::Result<()> {
    if std::env::args().any(|arg| arg == "--scripted") {
        scripted()
    } else {
        interactive()
    }
}
