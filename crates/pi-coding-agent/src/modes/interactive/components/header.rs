//! Header pieces: the pi logo, the built-in startup header (logo + key hints,
//! expandable with ctrl+o), and a re-export of [`DynamicBorder`] for the
//! surfaces that used to import it from here.
//!
//! Ground truth: `modes/interactive/components/pi-logo.ts`,
//! `dynamic-border.ts`, and the header construction in
//! `interactive-mode.ts:1049-1122` (`BuiltInHeader` extends `ExpandableText`).

use pi_tui::component::Component;
use pi_tui::mouse::{MouseEvent, MouseEventKind, MouseEventResult};

use super::super::theme;
pub use super::dynamic_border::DynamicBorder;

/// The brand colors (`pi-logo.ts:4-6`).
fn coral() -> pi_tui::color::Color {
    pi_tui::color::rgb_color(228.0, 138.0, 122.0)
}
fn blue() -> pi_tui::color::Color {
    pi_tui::color::rgb_color(79.0, 142.0, 179.0)
}
fn yellow() -> pi_tui::color::Color {
    pi_tui::color::rgb_color(234.0, 182.0, 93.0)
}

/// `piLogoLines` (`pi-logo.ts:19-26`): a 4×2 half-block logo. The brand
/// colors stay fixed across themes.
pub fn pi_logo_lines() -> (String, String) {
    let theme = theme::theme();
    let mode = theme.color_mode();
    let fg = |color: pi_tui::color::Color| pi_tui::color::foreground_ansi(color, mode);
    let bg = |color: pi_tui::color::Color| pi_tui::color::background_ansi(color, mode);
    // The fourth cell of the top line is empty, so it is padded to the same
    // width as the bottom line.
    let top = format!(
        "{}{}▀\x1b[0m{}▀█\x1b[0m ",
        fg(coral()),
        bg(blue()),
        fg(coral())
    );
    let bottom = format!("{}█▀\x1b[0m {}█\x1b[0m", fg(blue()), fg(yellow()));
    (top, bottom)
}

/// `piWordmark` (`pi-logo.ts:37-40`): the text fallback.
pub fn pi_wordmark() -> String {
    let theme = theme::theme();
    let mode = theme.color_mode();
    format!(
        "{}P\x1b[0m{}i\x1b[0m",
        pi_tui::color::foreground_ansi(coral(), mode),
        pi_tui::color::foreground_ansi(yellow(), mode)
    )
}

/// `ExpandableText` + `BuiltInHeader` (`interactive-mode.ts:245-275`): the
/// startup hints, compact until ctrl+o expands them.
pub struct BuiltInHeader {
    /// Compact text (logo line + one-line hints + onboarding notes).
    compact: String,
    /// Expanded text (logo line + the full hint list).
    expanded: String,
    pub expanded_state: bool,
    padding_x: usize,
    padding_y: usize,
    /// `header.onLogoClick` (`interactive-mode.ts:792`): the easter egg's
    /// callback, taking the logo's screen column and row. The mode queues
    /// the position for `pump`, which plays the 3D pi logo; unset, clicks
    /// pass through like upstream's unassigned callback.
    on_logo_click: Option<Box<dyn FnMut(i64, i64) + Send>>,
}

impl BuiltInHeader {
    pub fn new(
        compact: String,
        expanded: String,
        expanded_state: bool,
        padding_x: usize,
        padding_y: usize,
    ) -> Self {
        BuiltInHeader {
            compact,
            expanded,
            expanded_state,
            padding_x,
            padding_y,
            on_logo_click: None,
        }
    }

    /// `setExpanded` (`interactive-mode.ts:260-264`).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded_state = expanded;
    }

    /// `header.onLogoClick = …` (`interactive-mode.ts:792`).
    pub fn set_on_logo_click(&mut self, callback: Option<Box<dyn FnMut(i64, i64) + Send>>) {
        self.on_logo_click = callback;
    }
}

impl Component for BuiltInHeader {
    fn render(&self, width: usize) -> Vec<String> {
        let text = if self.expanded_state {
            &self.expanded
        } else {
            &self.compact
        };
        let mut lines = Vec::new();
        for _ in 0..self.padding_y {
            lines.push(String::new());
        }
        for line in text.split('\n') {
            let pad = " ".repeat(self.padding_x);
            lines.push(format!("{pad}{line}"));
        }
        for _ in 0..self.padding_y {
            lines.push(String::new());
        }
        let _ = width;
        lines
    }

    /// `BuiltInHeader.handleMouse` (`interactive-mode.ts:249-256`): a click
    /// on the logo — the four logo cells of the first two lines, after this
    /// component's padding column (upstream's header pads one column; the
    /// check follows its own padding) — hands the logo's screen position to
    /// the easter-egg callback.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseEventResult> {
        if event.kind != MouseEventKind::Click
            || event.y > 1
            || event.x < self.padding_x as i64
            || event.x > self.padding_x as i64 + 3
        {
            return None;
        }
        let callback = self.on_logo_click.as_mut()?;
        callback(event.screen_x - event.x + 1, event.screen_y - event.y);
        Some(MouseEventResult::handled())
    }

    /// The orchestrator toggles expansion through the tree downcast.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::ansi::strip_terminal_sequences;
    use std::sync::{Arc, Mutex};

    #[test]
    fn logo_is_two_lines_of_half_blocks() {
        let (top, bottom) = pi_logo_lines();
        let plain_top = strip_terminal_sequences(&top);
        let plain_bottom = strip_terminal_sequences(&bottom);
        // ▀▀█ padded with one space, then █▀ + space + █ — 4 cells per line
        // (the top line's fourth cell is empty, `pi-logo.ts:8-17`).
        assert_eq!(plain_top.chars().count(), 4, "{plain_top:?}");
        assert_eq!(plain_bottom.chars().count(), 4, "{plain_bottom:?}");
        assert!(plain_top.starts_with("▀▀█"));
        assert!(plain_bottom.starts_with("█▀"));
    }

    #[test]
    fn header_expands_and_collapses() {
        let mut header = BuiltInHeader::new(
            "compact".to_string(),
            "expanded\nmore".to_string(),
            false,
            0,
            0,
        );
        let rendered = header.render(40);
        assert_eq!(rendered.len(), 1);
        assert_eq!(strip_terminal_sequences(&rendered[0]), "compact");
        header.set_expanded(true);
        assert_eq!(header.render(40).len(), 2);
    }

    #[test]
    fn logo_clicks_reach_the_easter_egg_callback() {
        let mut header =
            BuiltInHeader::new("compact".to_string(), "expanded".to_string(), false, 0, 0);
        let hits = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&hits);
        header.set_on_logo_click(Some(Box::new(move |column, row| {
            sink.lock().expect("hits").push((column, row));
        })));
        // A click on the logo's bottom line, with the screen position offset
        // from the local one: the callback gets the logo's screen cell.
        let mut click = MouseEvent::new(MouseEventKind::Click, 0, 2, 1, 40, 2);
        click.screen_x = 10;
        click.screen_y = 5;
        header.handle_mouse(&click).expect("handled");
        assert_eq!(hits.lock().expect("hits").as_slice(), &[(9, 4)]);
        // Outside the logo cells: untouched.
        let outside = MouseEvent::new(MouseEventKind::Click, 0, 7, 1, 40, 2);
        assert!(header.handle_mouse(&outside).is_none());
        // Without a callback — the port's state until the 3D easter egg
        // lands — clicks pass through like upstream's unset onLogoClick.
        let mut bare = BuiltInHeader::new("c".to_string(), "e".to_string(), false, 0, 0);
        let click = MouseEvent::new(MouseEventKind::Click, 0, 2, 1, 40, 2);
        assert!(bare.handle_mouse(&click).is_none());
    }
}
