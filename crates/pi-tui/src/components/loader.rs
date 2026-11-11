//! `Loader`: a spinner line driven by the host clock.
//! `packages/tui/src/components/loader.ts`.
//!
//! pi owns an interval; this port owns a frame index. The host advances it
//! (`Loader::tick`) when `interval_ms` has passed, which is the only change:
//! pi's `start`/`stop`/`restartAnimation` lifecycle maps onto
//! `start`/`stop`/`tick`.

use crate::component::Component;
use crate::components::text::Text;

/// Animation frames. `loader.ts:21`.
pub const DEFAULT_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// `loader.ts:22`.
pub const DEFAULT_INTERVAL_MS: u64 = 80;

/// `loader.ts:12-16`.
#[derive(Debug, Clone)]
pub struct LoaderIndicator {
    pub frames: Vec<String>,
    pub interval_ms: u64,
}

impl Default for LoaderIndicator {
    fn default() -> Self {
        Self {
            frames: DEFAULT_FRAMES.iter().map(ToString::to_string).collect(),
            interval_ms: DEFAULT_INTERVAL_MS,
        }
    }
}

/// A spinner with a message. `loader.ts:27-...`.
pub struct Loader {
    text: Text,
    frames: Vec<String>,
    interval_ms: u64,
    current_frame: usize,
    running: bool,
    render_indicator_verbatim: bool,
    spinner_color: Box<dyn Fn(&str) -> String + Send + Sync>,
    message_color: Box<dyn Fn(&str) -> String + Send + Sync>,
    message: String,
    on_frame: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Loader {
    /// `loader.ts:35-49`.
    pub fn new(
        spinner_color: impl Fn(&str) -> String + Send + Sync + 'static,
        message_color: impl Fn(&str) -> String + Send + Sync + 'static,
        message: impl Into<String>,
        indicator: Option<LoaderIndicator>,
    ) -> Self {
        let mut loader = Self {
            // pi's Loader extends Text("", 1, 0): one column of side padding.
            text: Text::new("", 1, 0),
            frames: DEFAULT_FRAMES.iter().map(ToString::to_string).collect(),
            interval_ms: DEFAULT_INTERVAL_MS,
            current_frame: 0,
            running: false,
            render_indicator_verbatim: false,
            spinner_color: Box::new(spinner_color),
            message_color: Box::new(message_color),
            message: message.into(),
            on_frame: None,
        };
        loader.set_indicator(indicator);
        loader
    }

    /// Install the render requester. `loader.ts:41`.
    pub fn set_on_frame(&mut self, callback: Box<dyn Fn() + Send + Sync>) {
        self.on_frame = Some(callback);
    }

    /// `loader.ts:51-54`.
    pub fn start(&mut self) {
        self.running = true;
        self.update_display();
    }

    /// `loader.ts:56-61`.
    pub fn stop(&mut self) {
        self.running = false;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn current_frame(&self) -> &str {
        self.frames
            .get(self.current_frame)
            .map_or("", String::as_str)
    }

    /// `loader.ts:63-66`.
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.update_display();
    }

    /// `loader.ts:74-80`.
    pub fn set_indicator(&mut self, indicator: Option<LoaderIndicator>) {
        self.render_indicator_verbatim = indicator.is_some();
        let indicator = indicator.unwrap_or_default();
        self.frames = indicator.frames;
        self.interval_ms = if indicator.interval_ms > 0 {
            indicator.interval_ms
        } else {
            DEFAULT_INTERVAL_MS
        };
        self.current_frame = 0;
        self.start();
    }

    /// Advance the animation by one frame. Call at most every
    /// `interval_ms`; returns true when a new frame was rendered.
    /// `loader.ts:62-72` (`restartAnimation`'s interval callback).
    pub fn tick(&mut self) -> bool {
        if !self.running || self.frames.len() <= 1 {
            return false;
        }
        self.current_frame = (self.current_frame + 1) % self.frames.len();
        self.update_display();
        true
    }

    /// The interval the host should use to schedule [`Loader::tick`].
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// `loader.ts:82-86`.
    fn rendered_indicator(&self) -> String {
        let frame = self.current_frame().to_string();
        if self.render_indicator_verbatim {
            frame
        } else {
            (self.spinner_color)(&frame)
        }
    }

    /// `loader.ts:88-96`.
    fn update_display(&mut self) {
        let rendered = self.rendered_indicator();
        let indicator = if rendered.is_empty() {
            String::new()
        } else {
            format!("{rendered} ")
        };
        let message = (self.message_color)(&self.message);
        self.text.set_text(format!("{indicator}{message}"));
        if let Some(callback) = &self.on_frame {
            callback();
        }
    }
}

impl Component for Loader {
    /// `loader.ts:58-60`: one blank line above the spinner.
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = vec![String::new()];
        lines.extend(self.text.render(width));
        lines
    }

    fn invalidate(&mut self) {
        self.text.invalidate();
        self.update_display();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_terminal_sequences;

    fn loader(message: &str) -> Loader {
        Loader::new(ToString::to_string, ToString::to_string, message, None)
    }

    #[test]
    fn renders_a_blank_line_then_the_spinner() {
        let rendered = loader("Loading...").render(20);
        assert_eq!(rendered.len(), 2);
        assert_eq!(strip_terminal_sequences(&rendered[0]), "");
        assert!(strip_terminal_sequences(&rendered[1]).contains("Loading..."));
    }

    #[test]
    fn tick_advances_the_frame() {
        let mut spinner = loader("x");
        assert_eq!(spinner.current_frame(), "⠋");
        assert!(spinner.tick());
        assert_eq!(spinner.current_frame(), "⠙");
        assert!(spinner.tick());
        assert_eq!(spinner.current_frame(), "⠹");
    }

    #[test]
    fn tick_wraps_around() {
        let mut spinner = Loader::new(
            ToString::to_string,
            ToString::to_string,
            "x",
            Some(LoaderIndicator {
                frames: vec!["a".into(), "b".into()],
                interval_ms: 10,
            }),
        );
        assert_eq!(spinner.current_frame(), "a");
        spinner.tick();
        assert_eq!(spinner.current_frame(), "b");
        spinner.tick();
        assert_eq!(spinner.current_frame(), "a");
    }

    #[test]
    fn stopped_loaders_do_not_animate() {
        let mut spinner = loader("x");
        spinner.stop();
        assert!(!spinner.tick());
        assert_eq!(spinner.current_frame(), "⠋");
    }

    #[test]
    fn custom_frames_are_rendered_verbatim() {
        let spinner = Loader::new(
            |text| format!("<{text}>"),
            ToString::to_string,
            "x",
            Some(LoaderIndicator {
                frames: vec!["#".into()],
                interval_ms: 5,
            }),
        );
        let rendered = spinner.render(10);
        // The loader's Text has one column of side padding (`loader.ts:42`).
        assert_eq!(strip_terminal_sequences(&rendered[1]).trim(), "# x");
    }

    #[test]
    fn empty_frames_hide_the_indicator() {
        let spinner = Loader::new(
            ToString::to_string,
            ToString::to_string,
            "working",
            Some(LoaderIndicator {
                frames: Vec::new(),
                interval_ms: 5,
            }),
        );
        assert_eq!(
            strip_terminal_sequences(&spinner.render(10)[1]).trim(),
            "working"
        );
    }

    #[test]
    fn set_message_updates_the_line() {
        let mut spinner = loader("a");
        spinner.set_message("b");
        assert!(strip_terminal_sequences(&spinner.render(10)[1]).contains('b'));
    }
}
