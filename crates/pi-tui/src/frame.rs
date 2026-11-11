//! The offline frame-capture seam.
//!
//! pi renders to terminal lines and tests pi's TUI by capturing terminal
//! bytes (`tui-main-screen.ts:8-74`). This module adds the missing half: a
//! cell grid built from those lines, so a test can assert what the terminal
//! would show rather than which escapes were written.
//!
//! The grid is read-only. It never feeds the renderer: pi's renderer is line
//! based and this port keeps it that way.

use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::{AnsiCodeTracker, extract_ansi_code, grapheme_width, visible_width};

/// One terminal cell. Wide graphemes occupy two cells; the continuation
/// cell is [`Cell::continuation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The grapheme, or `""` for the second cell of a wide grapheme.
    pub text: String,
    /// Active SGR parameters, e.g. `1;38;5;244`. Empty when unstyled.
    pub style: String,
    /// OSC 8 hyperlink target, when the cell is inside a link.
    pub link: Option<String>,
    /// True for the second cell of a wide grapheme.
    pub continuation: bool,
}

impl Cell {
    fn blank() -> Self {
        Self {
            text: " ".to_string(),
            style: String::new(),
            link: None,
            continuation: false,
        }
    }
}

/// A rectangular grid of cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellGrid {
    pub width: usize,
    pub height: usize,
    pub cells: Vec<Cell>,
}

impl CellGrid {
    fn blank(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::blank(); width * height],
        }
    }

    /// The cell at `(column, row)`, or `None` when out of bounds.
    pub fn cell(&self, column: usize, row: usize) -> Option<&Cell> {
        if column >= self.width || row >= self.height {
            return None;
        }
        self.cells.get(row * self.width + column)
    }

    /// The row's text, with continuation cells dropped.
    pub fn row_text(&self, row: usize) -> String {
        (0..self.width)
            .filter_map(|column| self.cell(column, row))
            .filter(|cell| !cell.continuation)
            .map(|cell| cell.text.as_str())
            .collect()
    }

    /// Every row's text.
    pub fn rows(&self) -> Vec<String> {
        (0..self.height).map(|row| self.row_text(row)).collect()
    }

    /// Visible width of a row, ignoring the trailing blanks the grid pads with.
    pub fn row_width(&self, row: usize) -> usize {
        let row_text = self.row_text(row);
        row_text.trim_end_matches(' ').chars().count()
    }

    /// Rows whose text differs from `other`, as `(row, before, after)`.
    pub fn diff(&self, other: &CellGrid) -> Vec<(usize, String, String)> {
        let mut differences = Vec::new();
        for row in 0..self.height.min(other.height) {
            let before = self.row_text(row);
            let after = other.row_text(row);
            if before != after {
                differences.push((row, before, after));
            }
        }
        differences
    }
}

/// Build a cell grid from rendered lines.
///
/// Lines wider than `width` are truncated, matching what the terminal can
/// show; lines that are shorter are padded with blanks.
// `line` (the source row) and `link` (the hyperlink target) are distinct
// concepts the grid distinguishes cell by cell.
#[allow(clippy::similar_names)]
pub fn cell_grid(lines: &[String], width: usize, height: usize) -> CellGrid {
    let mut grid = CellGrid::blank(width, height);
    let mut tracker = AnsiCodeTracker::default();
    for row in 0..height {
        let Some(line) = lines.get(row) else {
            // Style state carries across lines, so keep tracking even when a
            // line is missing.
            continue;
        };
        let mut column = 0usize;
        let mut index = 0usize;
        while index < line.len() && column < width {
            if let Some(code) = extract_ansi_code(line, index) {
                tracker.process(code);
                index += code.len();
                continue;
            }
            let end = visible_run_end(line, index);
            let mut consumed = index;
            for segment in line[index..end].graphemes(true) {
                let cells = grapheme_width(segment);
                if cells == 0 {
                    // A combining mark merges into the previous cell.
                    consumed += segment.len();
                    continue;
                }
                if column >= width {
                    break;
                }
                let style = tracker.active_codes();
                let link: Option<String> = tracker.active_link().map(str::to_string);
                if let Some(slot) = grid.cells.get_mut(row * width + column) {
                    *slot = Cell {
                        text: segment.to_string(),
                        style: style.clone(),
                        link: link.clone(),
                        continuation: false,
                    };
                }
                for offset in 1..cells {
                    if let Some(slot) = grid.cells.get_mut(row * width + column + offset) {
                        *slot = Cell {
                            text: String::new(),
                            style: style.clone(),
                            link: link.clone(),
                            continuation: true,
                        };
                    }
                }
                column += cells;
                consumed += segment.len();
            }
            index = if column >= width {
                end
            } else {
                consumed.max(end)
            };
        }
    }
    grid
}

/// The visible width of a line, ignoring styling.
pub fn line_width(line: &str) -> usize {
    visible_width(line)
}

/// End of the run of visible bytes starting at `i`, skipping escapes.
fn visible_run_end(s: &str, i: usize) -> usize {
    let start = match extract_ansi_code(s, i) {
        Some(code) => i + code.len(),
        None => i,
    };
    let mut end = start;
    while end < s.len() && extract_ansi_code(s, end).is_none() {
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(lines: &[&str], width: usize) -> CellGrid {
        cell_grid(
            &lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            width,
            lines.len(),
        )
    }

    #[test]
    fn plain_lines_become_text() {
        let grid = grid(&["ab", "cd"], 4);
        assert_eq!(grid.rows(), vec!["ab  ", "cd  "]);
        assert_eq!(grid.row_width(0), 2);
    }

    #[test]
    fn missing_rows_are_blank() {
        let grid = cell_grid(&["a".to_string()], 2, 3);
        assert_eq!(grid.rows(), vec!["a ", "  ", "  "]);
    }

    #[test]
    fn styling_is_attached_to_cells() {
        let grid = grid(&["\x1b[1mab"], 2);
        assert_eq!(grid.cell(0, 0).unwrap().style, "\x1b[1m");
        assert_eq!(grid.cell(1, 0).unwrap().style, "\x1b[1m");
    }

    #[test]
    fn style_resets_between_cells() {
        let grid = grid(&["\x1b[31ma\x1b[39mb"], 2);
        assert_eq!(grid.cell(0, 0).unwrap().style, "\x1b[31m");
        assert_eq!(grid.cell(1, 0).unwrap().style, "");
    }

    #[test]
    fn wide_graphemes_occupy_two_cells() {
        let grid = grid(&["日本"], 4);
        assert_eq!(grid.row_text(0), "日本");
        assert!(!grid.cell(0, 0).unwrap().continuation);
        assert!(grid.cell(1, 0).unwrap().continuation);
        assert_eq!(grid.cell(2, 0).unwrap().text, "本");
        assert!(grid.cell(3, 0).unwrap().continuation);
    }

    #[test]
    fn hyperlinks_are_captured() {
        let grid = grid(&["\x1b]8;;https://example.com\x07link\x1b]8;;\x07"], 4);
        assert_eq!(
            grid.cell(0, 0).unwrap().link.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(grid.row_text(0), "link");
    }

    #[test]
    fn overlong_lines_are_truncated_to_the_viewport() {
        let grid = grid(&["abcdef"], 3);
        assert_eq!(grid.row_text(0), "abc");
    }

    #[test]
    fn out_of_bounds_access_is_none() {
        let grid = grid(&["a"], 1);
        assert!(grid.cell(1, 0).is_none());
        assert!(grid.cell(0, 9).is_none());
    }

    #[test]
    fn diff_reports_only_changed_rows() {
        let before = grid(&["a", "b", "c"], 3);
        let after = grid(&["a", "B", "c"], 3);
        let differences = before.diff(&after);
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].0, 1);
        assert_eq!(differences[0].1, "b  ");
        assert_eq!(differences[0].2, "B  ");
    }
}
