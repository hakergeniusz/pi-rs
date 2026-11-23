//! `Markdown`: pi's assistant-message renderer.
//!
//! Ground truth: `packages/tui/src/components/markdown.ts` (1,025 lines).
//!
//! pi parses with `marked` and then renders its token tree
//! (`renderToken`/`renderInlineTokens`, `markdown.ts:465-782`) into styled
//! lines, wraps each with `wrapTextWithAnsi`, adds `paddingX` margins and
//! `paddingY` blank lines. This port keeps that **rendering model and its
//! layout** — block order, blank-line rules, bullets, fences, quote bars, table
//! boxes, wrapping, padding — with a hand-written block/inline parser instead
//! of `marked`. Fidelity target is the layout pi produces, not byte-identical
//! ANSI: the theme functions are the host's, so the escapes come from
//! [`crate::color`] in the interactive mode.
//!
//! LaTeX tokens and display math are ported in [`markdown_extras`] from
//! `markdown.ts:37-150` and a documented subset of `latex.ts`; links use OSC 8
//! when the terminal advertises hyperlinks, falling back to pi's parenthesised
//! URL, and image lines are passed through unwrapped like upstream.
//!
//! Deviations, all documented in `markdown_extras`' module docs:
//!
//! * The math renderer is a faithful subset of upstream's 1,506-line renderer:
//!   no matrices, `\mathbb`, `\binom`, `\boxed`, `\not` or 2-D script layout.
//! * Tables are ported, because their layout is deterministic; the `Space`
//!   token that `marked` inserts for blank lines inside a list is reproduced by
//!   the parser's `loose` detection rather than by marked's own pass.
//! * Upstream's highlight.js grammars are replaced by the dependency-free
//!   lexer in [`markdown_extras`] (json, rust, ts/js, shell, python).

use std::sync::Arc;

use crate::ansi::{apply_background_to_line, visible_width, wrap_text_with_ansi};
use crate::component::Component;

/// LaTeX tokenization/rendering and syntax highlighting.
/// Declared here so the port keeps all markdown extras with the component.
#[path = "../markdown_extras.rs"]
pub mod markdown_extras;

use markdown_extras::{
    block_token_line_count, render_latex, tokenize_block_latex, tokenize_inline_latex,
};

type Style = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// pi's `theme.highlightCode` signature (`markdown.ts:233`): the language and
/// the fence's info string in, highlighted lines out.
pub type CodeHighlighter = dyn Fn(&str, Option<&str>) -> Vec<String> + Send + Sync + 'static;

fn identity() -> Style {
    Arc::new(|text: &str| text.to_string())
}

/// pi's `MarkdownTheme` (`markdown.ts:207-234`). Every hook is a function from
/// text to styled text, exactly as in pi.
#[derive(Clone)]
pub struct MarkdownTheme {
    /// Style hook for heading text.
    pub heading: Style,
    /// Style hook for link text.
    pub link: Style,
    /// Style hook for the URL printed after a link label.
    pub link_url: Style,
    /// Style hook for inline code spans.
    pub code: Style,
    /// Style hook for code-block body lines.
    pub code_block: Style,
    /// Style hook for the fence lines around a code block.
    pub code_block_border: Style,
    /// Style hook for block-quote text.
    pub quote: Style,
    /// Style hook for the `│ ` quote bar.
    pub quote_border: Style,
    /// Style hook for horizontal rules.
    pub hr: Style,
    /// Style hook for list bullets.
    pub list_bullet: Style,
    /// Style hook for bold runs.
    pub bold: Style,
    /// Style hook for italic runs.
    pub italic: Style,
    /// Style hook for strikethrough runs.
    pub strikethrough: Style,
    /// Style hook for underlined runs; links use it too.
    pub underline: Style,
    /// `theme.highlightCode` (`markdown.ts:233`), optional. When absent each
    /// code line goes through `code_block`, which is pi's own fallback.
    pub highlight_code: Option<Arc<CodeHighlighter>>,
    /// `theme.codeBlockIndent`, default `"  "`.
    pub code_block_indent: String,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        Self::plain()
    }
}

impl std::fmt::Debug for MarkdownTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MarkdownTheme")
    }
}

impl MarkdownTheme {
    /// Every hook is the identity: unstyled text, which is what an offline
    /// layout snapshot wants.
    pub fn plain() -> MarkdownTheme {
        MarkdownTheme {
            heading: identity(),
            link: identity(),
            link_url: identity(),
            code: identity(),
            code_block: identity(),
            code_block_border: identity(),
            quote: identity(),
            quote_border: identity(),
            hr: identity(),
            list_bullet: identity(),
            bold: identity(),
            italic: identity(),
            strikethrough: identity(),
            underline: identity(),
            highlight_code: None,
            code_block_indent: "  ".to_string(),
        }
    }
}

/// pi's `DefaultTextStyle` (`markdown.ts:175-193`): the message-level styling
/// applied to text, with the background handled separately at the padding
/// stage.
// The bool flags mirror pi's `DefaultTextStyle` fields one-to-one
// (`markdown.ts:175-193`).
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Default)]
pub struct DefaultTextStyle {
    /// Style hook for the base text color.
    pub color: Option<Style>,
    /// Apply the theme's bold hook to base text.
    pub bold: bool,
    /// Apply the theme's italic hook to base text.
    pub italic: bool,
    /// Apply the theme's strikethrough hook to base text.
    pub strikethrough: bool,
    /// Apply the theme's underline hook to base text.
    pub underline: bool,
    /// Background painter applied at the padding stage.
    pub bg_color: Option<Style>,
}

/// The render-time markdown hook (`markdown.ts:289`): receives the source
/// and the content width, returns the markdown to render. The interactive
/// mode chains extension transformers through it (E3 slice 2).
pub type MarkdownTransform = std::sync::Arc<dyn Fn(&str, usize) -> String + Send + Sync>;

/// pi's `MarkdownOptions` (`markdown.ts:236-249`); only the parts this port
/// implements.
#[derive(Clone, Default)]
pub struct MarkdownOptions {
    /// Keep the source markers (`1)`, `*`) instead of normalizing to `.`/`-`.
    pub preserve_list_markers: bool,
    /// Keep `\*` escapes in the output.
    pub preserve_backslash_escapes: bool,
    /// `transform` (`markdown.ts:289`): applied per render with the content
    /// width (`markdown.ts:376`), so a reused component sees stream state
    /// and width changes.
    pub transform: Option<MarkdownTransform>,
}

impl std::fmt::Debug for MarkdownOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transform is a closure — render it as a marker only.
        f.debug_struct("MarkdownOptions")
            .field("preserve_list_markers", &self.preserve_list_markers)
            .field(
                "preserve_backslash_escapes",
                &self.preserve_backslash_escapes,
            )
            .field("transform", &self.transform.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------- inline AST

/// Inline token. `markdown.ts:180-206` (`DefaultTextStyle` neighbour types)
/// and the inline switch at `:661-782`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Em(Vec<Inline>),
    Del(Vec<Inline>),
    CodeSpan(String),
    Link {
        text: Vec<Inline>,
        href: String,
    },
    /// pi's `latex` token (`markdown.ts:37-41, 63-110`).
    Latex {
        text: String,
        raw: String,
        pending: bool,
    },
    /// pi's `br`.
    Break,
}

/// Block token. `renderToken`'s switch (`markdown.ts:465-634`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading {
        depth: usize,
        content: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    Code {
        lang: String,
        text: String,
    },
    /// pi's `latexBlock` token (`markdown.ts:128-150`).
    LatexBlock {
        text: String,
        raw: String,
        pending: bool,
    },
    List(List),
    BlockQuote(Vec<Block>),
    Table(Table),
    Hr,
    /// pi's `space` token: one blank line.
    Space,
    /// Rendered as plain text; pi renders `html` tokens' raw source.
    Html(String),
}

/// A list. `Tokens.List`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    /// Ordered (`1.`) versus bulleted (`-`) list.
    pub ordered: bool,
    /// The first marker's number.
    pub start: usize,
    /// The list's items.
    pub items: Vec<ListItem>,
    /// pi's `list.loose`: blank lines between items render a spacer.
    pub loose: bool,
}

/// A list item. `Tokens.ListItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// The source marker (`-`, `*`, `1.`), when markers are preserved.
    pub marker: Option<String>,
    /// pi's `item.task` / `item.checked`.
    pub task: Option<bool>,
    /// The item's block content.
    pub blocks: Vec<Block>,
}

/// A table. `Tokens.Table`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// The header row's cells.
    pub header: Vec<Vec<Inline>>,
    /// The body rows, one cell vector per row.
    pub rows: Vec<Vec<Vec<Inline>>>,
    /// `token.raw`, used as the fallback when the table cannot be laid out.
    pub raw: String,
}

// ------------------------------------------------------------------ parsing

/// pi's `StrictStrikethroughTokenizer` (`markdown.ts:6-20`): `~~text~~` only
/// counts when the delimiters are not followed by whitespace.
fn parse_delimited(text: &str, marker: &str) -> Option<(String, usize)> {
    let start = text.find(marker)?;
    let after_open = start + marker.len();
    let content_start = text[after_open..].chars().next()?;
    if content_start.is_whitespace() {
        return None;
    }
    let bytes = text.as_bytes();
    let mut index = after_open;
    while index < bytes.len() {
        if text[index..].starts_with(marker) {
            let end = index;
            let next = text[end + marker.len()..].chars().next();
            if next.is_none_or(|c| c != '~') {
                let content = text[after_open..end].to_string();
                if content.trim_end().is_empty() {
                    return None;
                }
                return Some((content, end + marker.len()));
            }
        }
        index += 1;
    }
    None
}

/// Parse inline tokens out of `text`. The block-level constructs pi delegates
/// to `marked` (links, emphasis, code spans, escapes, hard breaks) are handled
/// here; LaTeX is not.
// Long because it mirrors pi's inline tokenizer (`markdown.ts:465-782`) as one
// scanner loop.
#[allow(clippy::too_many_lines)]
pub fn parse_inline(text: &str) -> Vec<Inline> {
    let mut tokens = Vec::new();
    let mut buffer = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;

    let flush = |buffer: &mut String, tokens: &mut Vec<Inline>| {
        if !buffer.is_empty() {
            tokens.push(Inline::Text(std::mem::take(buffer)));
        }
    };

    while index < chars.len() {
        let c = chars[index];
        match c {
            '\\' if matches!(chars.get(index + 1), Some('(' | '[')) => {
                // pi's inline LaTeX extension (`markdown.ts:163-172`).
                let rest: String = chars[index..].iter().collect();
                if let Some(token) = tokenize_inline_latex(&rest) {
                    flush(&mut buffer, &mut tokens);
                    tokens.push(Inline::Latex {
                        text: token.text,
                        raw: token.raw.clone(),
                        pending: token.pending,
                    });
                    index += token.raw.chars().count();
                    continue;
                }
                buffer.push(c);
                index += 1;
            }
            '$' => {
                let rest: String = chars[index..].iter().collect();
                if let Some(token) = tokenize_inline_latex(&rest) {
                    flush(&mut buffer, &mut tokens);
                    tokens.push(Inline::Latex {
                        text: token.text,
                        raw: token.raw.clone(),
                        pending: token.pending,
                    });
                    index += token.raw.chars().count();
                    continue;
                }
                buffer.push(c);
                index += 1;
            }
            '\\' => {
                // pi's `escape` token.
                if index + 1 < chars.len() {
                    let next = chars[index + 1];
                    if next == '\n' {
                        flush(&mut buffer, &mut tokens);
                        tokens.push(Inline::Break);
                        index += 2;
                        continue;
                    }
                    if is_escapable(next) {
                        buffer.push(next);
                        index += 2;
                        continue;
                    }
                }
                buffer.push(c);
                index += 1;
            }
            '`' => {
                let run = chars[index..].iter().take_while(|c| **c == '`').count();
                let marker: String = "`".repeat(run);
                let rest: String = chars[index + run..].iter().collect();
                if let Some(end) = rest.find(&marker) {
                    let code = &rest[..end];
                    flush(&mut buffer, &mut tokens);
                    tokens.push(Inline::CodeSpan(code.to_string()));
                    index += run + code.chars().count() + run;
                    continue;
                }
                buffer.push_str(&marker);
                index += run;
            }
            '[' => {
                let rest: String = chars[index..].iter().collect();
                if let Some((label, href, consumed)) = parse_link(&rest) {
                    flush(&mut buffer, &mut tokens);
                    tokens.push(Inline::Link {
                        text: parse_inline(&label),
                        href,
                    });
                    index += consumed;
                    continue;
                }
                buffer.push(c);
                index += 1;
            }
            '~' if chars.get(index + 1) == Some(&'~') => {
                let rest: String = chars[index..].iter().collect();
                if let Some((content, consumed)) = parse_delimited(&rest, "~~") {
                    flush(&mut buffer, &mut tokens);
                    tokens.push(Inline::Del(parse_inline(&content)));
                    index += consumed;
                } else {
                    buffer.push(c);
                    index += 1;
                }
            }
            '*' | '_' if chars.get(index + 1) == Some(&c) => {
                let marker: String = c.to_string().repeat(2);
                let rest: String = chars[index..].iter().collect();
                if let Some((content, consumed)) = parse_delimited(&rest, &marker) {
                    flush(&mut buffer, &mut tokens);
                    tokens.push(Inline::Strong(parse_inline(&content)));
                    index += consumed;
                } else {
                    buffer.push(c);
                    index += 1;
                }
            }
            '*' | '_' => {
                let rest: String = chars[index..].iter().collect();
                if let Some((content, consumed)) = parse_delimited(&rest, c.to_string().as_str()) {
                    flush(&mut buffer, &mut tokens);
                    tokens.push(Inline::Em(parse_inline(&content)));
                    index += consumed;
                } else {
                    buffer.push(c);
                    index += 1;
                }
            }
            '\n' => {
                // Two trailing spaces make a hard break, pi's `br`.
                let trailing_spaces = buffer.chars().rev().take_while(|c| *c == ' ').count();
                if trailing_spaces >= 2 {
                    let keep = buffer.chars().count() - trailing_spaces;
                    buffer = buffer.chars().take(keep).collect();
                    flush(&mut buffer, &mut tokens);
                    tokens.push(Inline::Break);
                } else {
                    buffer.push('\n');
                }
                index += 1;
            }
            _ => {
                buffer.push(c);
                index += 1;
            }
        }
    }
    flush(&mut buffer, &mut tokens);
    tokens
}

fn is_escapable(c: char) -> bool {
    c.is_ascii_punctuation()
}

/// `[label](href)`. Returns the label, the href and how many chars the whole
/// construct took.
fn parse_link(text: &str) -> Option<(String, String, usize)> {
    let mut depth = 0usize;
    let chars: Vec<char> = text.chars().collect();
    let mut label_end = None;
    for (index, c) in chars.iter().enumerate() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    label_end = Some(index);
                    break;
                }
            }
            _ => {}
        }
        if depth == 0 {
            return None;
        }
    }
    let label_end = label_end?;
    if chars.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let href_start = label_end + 2;
    let href_end = href_start + chars[href_start..].iter().position(|char| *char == ')')?;
    let label: String = chars[1..label_end].iter().collect();
    let href: String = chars[href_start..href_end].iter().collect();
    Some((label, href.trim().to_string(), href_end + 1))
}

/// The block-level token name `renderToken` branches on, for the "is the next
/// token a space/list" spacing rules.
fn block_type(block: &Block) -> &'static str {
    match block {
        Block::Heading { .. } => "heading",
        Block::Paragraph(_) => "paragraph",
        Block::Code { .. } => "code",
        Block::LatexBlock { .. } => "latexBlock",
        Block::List(_) => "list",
        Block::BlockQuote(_) => "blockquote",
        Block::Table(_) => "table",
        Block::Hr => "hr",
        Block::Space => "space",
        Block::Html(_) => "html",
    }
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn leading_spaces(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn is_hr(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.len() < 3 {
        return false;
    }
    let first = trimmed.chars().next().unwrap_or(' ');
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let mut count = 0;
    for c in trimmed.chars() {
        if c == first {
            count += 1;
        } else if c != ' ' && c != '\t' {
            return false;
        }
    }
    count >= 3
}

/// The list marker at the start of `line`, as `(marker_text, content, indent)`.
fn list_marker(line: &str) -> Option<(String, String, usize)> {
    let indent = leading_spaces(line);
    let rest: String = line.chars().skip(indent).collect();
    let mut chars = rest.chars();
    let first = chars.next()?;
    let (marker, after_marker) = if matches!(first, '-' | '+' | '*') {
        (first.to_string(), rest.chars().skip(1).collect::<String>())
    } else if first.is_ascii_digit() {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let after_digits: String = rest.chars().skip(digits.len()).collect();
        let terminator = after_digits.chars().next()?;
        if !matches!(terminator, '.' | ')') {
            return None;
        }
        let marker = format!("{digits}{terminator}");
        (marker, after_digits.chars().skip(1).collect::<String>())
    } else {
        return None;
    };
    let spaces = after_marker
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .count();
    if spaces == 0 {
        // A marker must be followed by whitespace or end the line: `**bold**`
        // is emphasis, not a bullet (`editor.ts` marked's list rule).
        return None;
    }
    let content: String = after_marker.chars().skip(spaces).collect();
    Some((format!("{marker} "), content, indent))
}

/// pi's `trimPartialClosingFences` (`markdown.ts:330`): a fenced block that
/// was never closed still ends at the end of the message.
fn fence_marker(line: &str) -> Option<(char, usize, String)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let trimmed: String = line.chars().skip(indent).collect();
    let first = trimmed.chars().next()?;
    if !matches!(first, '`' | '~') {
        return None;
    }
    let run = trimmed.chars().take_while(|c| *c == first).count();
    if run < 3 {
        return None;
    }
    let info: String = trimmed.chars().skip(run).collect();
    // A backtick fence's info string may not contain a backtick.
    if first == '`' && info.contains('`') {
        return None;
    }
    Some((first, run, info.trim().to_string()))
}

/// Split a table row into cells, honouring escaped pipes.
fn split_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '|' => cells.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    cells.push(current);
    cells
        .into_iter()
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// pi's table delimiter row (`| --- | :-: |`).
fn is_delimiter_row(line: &str) -> bool {
    if !line.contains('-') {
        return false;
    }
    split_row(line)
        .iter()
        .all(|cell| !cell.is_empty() && cell.chars().all(|c| matches!(c, '-' | ':' | ' ')))
}

/// Parse a markdown source into pi's block list.
pub fn parse_blocks(source: &str) -> Vec<Block> {
    let lines: Vec<&str> = source.split('\n').collect();
    parse_block_lines(&lines)
}

// Long because it mirrors `marked`'s lexer switch (`markdown.ts:465-634`)
// branch for branch.
#[allow(clippy::too_many_lines)]
fn parse_block_lines(lines: &[&str]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        // pi's block LaTeX extension (`markdown.ts:152-161`) runs ahead of the
        // ordinary block tokenizer at a line boundary. The prefix guard is the
        // extension's `start` probe; it avoids re-joining the source per line.
        let latex_start = leading_spaces(line) <= 3
            && (line.trim_start_matches(' ').starts_with("$$")
                || line.trim_start_matches(' ').starts_with("\\["));
        if latex_start {
            let remaining = lines[index..].join("\n");
            if let Some(token) = tokenize_block_latex(&remaining) {
                let consumed = block_token_line_count(&token).min(lines.len() - index);
                blocks.push(Block::LatexBlock {
                    text: token.text,
                    raw: token.raw,
                    pending: token.pending,
                });
                index += consumed.max(1);
                continue;
            }
        }
        if is_blank(line) {
            // `marked`'s lexer does not turn the blank line between two blocks
            // into a `space` token: blocks are separated by their own trailing
            // blank line (`renderToken`), and `Space` only survives where the
            // list renderer puts it (`markdown.ts:610`). A list body keeps its
            // blanks, which is where `parse_list`'s `loose` flag reads them.
            index += 1;
            continue;
        }
        if let Some((fence_char, run, info)) = fence_marker(line) {
            let indent = leading_spaces(line);
            let mut body = Vec::new();
            index += 1;
            let mut closed = false;
            while index < lines.len() {
                let candidate = lines[index];
                let candidate_indent = leading_spaces(candidate);
                let candidate_trimmed: String = candidate.chars().skip(candidate_indent).collect();
                let candidate_run = candidate_trimmed
                    .chars()
                    .take_while(|c| *c == fence_char)
                    .count();
                if candidate_indent <= 3
                    && candidate_run >= run
                    && candidate_trimmed
                        .chars()
                        .skip(candidate_run)
                        .all(char::is_whitespace)
                {
                    closed = true;
                    index += 1;
                    break;
                }
                // Strip the opening fence's indent, like marked does.
                body.push(
                    candidate
                        .chars()
                        .skip(indent.min(candidate_indent))
                        .collect::<String>(),
                );
                index += 1;
            }
            let _ = closed;
            let mut text = body.join("\n");
            // marked keeps a trailing newline inside the code token; pi splits
            // on it and therefore shows a final empty line only for explicit
            // blank lines.
            if text.ends_with('\n') {
                text.pop();
            }
            blocks.push(Block::Code { lang: info, text });
            continue;
        }
        if is_hr(line) {
            blocks.push(Block::Hr);
            index += 1;
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes) && trimmed.chars().nth(hashes).is_some_and(|c| c == ' ') {
                let content: String = trimmed.chars().skip(hashes + 1).collect();
                blocks.push(Block::Heading {
                    depth: hashes,
                    content: parse_inline(content.trim_end()),
                });
                index += 1;
                continue;
            }
        }
        if trimmed.starts_with('>') {
            let mut inner = Vec::new();
            while index < lines.len() {
                let candidate = lines[index].trim_start();
                if !candidate.starts_with('>') {
                    break;
                }
                let stripped = candidate.strip_prefix('>').unwrap_or("");
                inner.push(stripped.strip_prefix(' ').unwrap_or(stripped));
                index += 1;
            }
            let inner_refs: Vec<&str> = inner.clone();
            blocks.push(Block::BlockQuote(parse_block_lines(&inner_refs)));
            continue;
        }
        if line.contains('|') && index + 1 < lines.len() && is_delimiter_row(lines[index + 1]) {
            let header_cells = split_row(line);
            let columns = header_cells.len();
            index += 2;
            let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
            let mut raw = format!("{}\n{}", lines[index - 2], lines[index - 1]);
            while index < lines.len() && !is_blank(lines[index]) && lines[index].contains('|') {
                let cells = split_row(lines[index]);
                raw.push('\n');
                raw.push_str(lines[index]);
                index += 1;
                let mut row: Vec<Vec<Inline>> = Vec::new();
                for position in 0..columns {
                    let cell = cells.get(position).cloned().unwrap_or_default();
                    row.push(parse_inline(&cell));
                }
                rows.push(row);
            }
            blocks.push(Block::Table(Table {
                header: header_cells.iter().map(|cell| parse_inline(cell)).collect(),
                rows,
                raw,
            }));
            continue;
        }
        if list_marker(line).is_some() {
            let (list, consumed) = parse_list(lines, index);
            blocks.push(list);
            // Progress is a precondition, not an expectation: a list parser
            // that returned zero consumed lines would loop here forever.
            index += if consumed == 0 { 1 } else { consumed };
            continue;
        }
        // Paragraph: consecutive non-blank, non-block lines joined by newlines,
        // exactly as `marked` does before pi wraps them.
        let mut paragraph = String::new();
        while index < lines.len() {
            let candidate = lines[index];
            if is_blank(candidate)
                || fence_marker(candidate).is_some()
                || is_hr(candidate)
                || candidate.trim_start().starts_with('#')
                || candidate.trim_start().starts_with('>')
                || list_marker(candidate).is_some()
            {
                break;
            }
            if !paragraph.is_empty() {
                paragraph.push('\n');
            }
            paragraph.push_str(candidate);
            index += 1;
        }
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(parse_inline(&paragraph)));
        }
    }
    blocks
}

/// Parse a list starting at `start`; returns the list and how many lines it
/// took. Nesting is by indent: a line indented at least `marker_width + 2`
/// belongs to the item, and an item's own markers set the baseline.
// Long because it mirrors pi's list tokenizer (`markdown.ts:415-463`) line for line.
#[allow(clippy::too_many_lines)]
fn parse_list(lines: &[&str], start: usize) -> (Block, usize) {
    let (first_marker, _, first_indent) = list_marker(lines[start]).expect("a list marker");
    let ordered = first_marker
        .trim_end()
        .chars()
        .next()
        .unwrap_or('-')
        .is_ascii_digit();
    let start_number = first_marker
        .trim_end()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse::<usize>()
        .unwrap_or(1);
    let mut items: Vec<ListItem> = Vec::new();
    let mut loose = false;
    let mut index = start;
    let mut saw_blank_between_items = false;

    while index < lines.len() {
        let Some((marker, content, indent)) = list_marker(lines[index]) else {
            break;
        };
        if indent > first_indent + 3 {
            break;
        }
        let item_is_ordered = marker
            .trim_end()
            .chars()
            .next()
            .unwrap_or('-')
            .is_ascii_digit();
        if item_is_ordered != ordered {
            break;
        }
        // A marker at a shallower indent starts the next list; a deeper one is
        // this item's nested content, which the item body loop below collects.
        if indent < first_indent && !items.is_empty() {
            break;
        }
        if saw_blank_between_items {
            loose = true;
        }
        let (task, content) = split_task(&content);
        let mut body: Vec<String> = vec![content];
        let body_indent = indent + marker.chars().count();
        index += 1;
        let mut pending_blank = false;
        while index < lines.len() {
            let candidate = lines[index];
            if is_blank(candidate) {
                // A blank line ends the item unless the next line continues it.
                let next_is_content = lines.get(index + 1).is_some_and(|next| {
                    !is_blank(next)
                        && (leading_spaces(next) >= body_indent || list_marker(next).is_some())
                });
                if next_is_content {
                    loose = true;
                    body.push(String::new());
                    index += 1;
                    continue;
                }
                pending_blank = true;
                saw_blank_between_items = true;
                index += 1;
                continue;
            }
            let candidate_indent = leading_spaces(candidate);
            if list_marker(candidate).is_some() && candidate_indent < body_indent && pending_blank {
                break;
            }
            if list_marker(candidate).is_some() && candidate_indent <= indent {
                break;
            }
            if candidate_indent < body_indent && pending_blank {
                break;
            }
            if pending_blank {
                pending_blank = false;
            }
            let stripped: String = candidate
                .chars()
                .skip(candidate_indent.min(body_indent))
                .collect();
            body.push(stripped);
            index += 1;
        }
        let mut blocks = Vec::new();
        let body_refs: Vec<&str> = body.iter().map(String::as_str).collect();
        blocks.extend(parse_block_lines(&body_refs));
        items.push(ListItem {
            marker: Some(marker.trim_end().to_string()),
            task,
            blocks,
        });
    }

    (
        Block::List(List {
            ordered,
            start: start_number,
            items,
            loose,
        }),
        index - start,
    )
}

/// pi's task-list marker: `[ ] ` / `[x] ` (`markdown.ts:772-776`).
fn split_task(content: &str) -> (Option<bool>, String) {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() < 3 || chars[0] != '[' || chars[2] != ']' {
        return (None, content.to_string());
    }
    let checked = match chars[1] {
        'x' | 'X' => true,
        ' ' => false,
        _ => return (None, content.to_string()),
    };
    let rest: String = chars[3..].iter().collect();
    (
        Some(checked),
        rest.strip_prefix(' ').unwrap_or(&rest).to_string(),
    )
}

// ---------------------------------------------------------------- rendering

/// pi's `Markdown` component (`markdown.ts:236-...`).
pub struct Markdown {
    text: String,
    padding_x: usize,
    padding_y: usize,
    theme: MarkdownTheme,
    default_text_style: Option<DefaultTextStyle>,
    options: MarkdownOptions,
}

impl Markdown {
    /// `markdown.ts:263-277`.
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        default_text_style: Option<DefaultTextStyle>,
        options: MarkdownOptions,
    ) -> Markdown {
        Markdown {
            text: text.into(),
            padding_x,
            padding_y,
            theme,
            default_text_style,
            options,
        }
    }

    /// Replace the markdown source; the next render uses it.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    /// The markdown source.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// `markdown.ts:410-430`: the base styling applied to text content. The
    /// background is deliberately *not* applied here.
    fn apply_default_style(&self, text: &str) -> String {
        let Some(style) = &self.default_text_style else {
            return text.to_string();
        };
        let mut styled = text.to_string();
        if let Some(color) = &style.color {
            styled = color(&styled);
        }
        if style.bold {
            styled = (self.theme.bold)(&styled);
        }
        if style.italic {
            styled = (self.theme.italic)(&styled);
        }
        if style.strikethrough {
            styled = (self.theme.strikethrough)(&styled);
        }
        if style.underline {
            styled = (self.theme.underline)(&styled);
        }
        styled
    }

    /// `markdown.ts:432-448`: the escapes a style function opens, so nested
    /// tokens can restore it after their own reset.
    fn style_prefix(style: &Style) -> String {
        let sentinel = "\u{0}";
        let styled = style(sentinel);
        match styled.find(sentinel) {
            Some(index) => styled[..index].to_string(),
            None => String::new(),
        }
    }

    /// `markdown.ts:636-782`.
    fn render_inline(
        &self,
        tokens: &[Inline],
        style_prefix: &str,
        apply_text: &dyn Fn(&str) -> String,
    ) -> String {
        let mut result = String::new();
        for token in tokens {
            match token {
                Inline::Text(text) => result.push_str(&apply_text(text)),
                Inline::Break => result.push('\n'),
                Inline::Strong(children) => {
                    let inner = self.render_inline(children, style_prefix, apply_text);
                    result.push_str(&(self.theme.bold)(&inner));
                    result.push_str(style_prefix);
                }
                Inline::Em(children) => {
                    let inner = self.render_inline(children, style_prefix, apply_text);
                    result.push_str(&(self.theme.italic)(&inner));
                    result.push_str(style_prefix);
                }
                Inline::Del(children) => {
                    let inner = self.render_inline(children, style_prefix, apply_text);
                    result.push_str(&(self.theme.strikethrough)(&inner));
                    result.push_str(style_prefix);
                }
                Inline::CodeSpan(text) => {
                    result.push_str(&(self.theme.code)(text));
                    result.push_str(style_prefix);
                }
                Inline::Link { text, href } => {
                    let link_text = self.render_inline(text, style_prefix, apply_text);
                    let styled_link = (self.theme.link)(&(self.theme.underline)(&link_text));
                    if crate::terminal_image::get_capabilities().hyperlinks {
                        // OSC 8 (`markdown.ts:836-840`): the URL is not printed
                        // inline; the styled label itself is made clickable.
                        result.push_str(&crate::terminal_image::hyperlink(&styled_link, href));
                    } else {
                        // pi's non-hyperlink fallback (`markdown.ts:841-851`):
                        // print the URL when it differs from the label.
                        let plain_label: String = text
                            .iter()
                            .filter_map(|token| match token {
                                Inline::Text(text) => Some(text.as_str()),
                                _ => None,
                            })
                            .collect();
                        let href_for_comparison = href.strip_prefix("mailto:").unwrap_or(href);
                        if plain_label == *href || plain_label == href_for_comparison {
                            result.push_str(&styled_link);
                        } else {
                            result.push_str(&styled_link);
                            result.push_str(&(self.theme.link_url)(&format!(" ({href})")));
                        }
                    }
                    result.push_str(style_prefix);
                }
                Inline::Latex { text, raw, pending } => {
                    // `markdown.ts:789-796`: pending expressions stay as source;
                    // unsupported/malformed math falls back to the raw token.
                    let rendered = if *pending {
                        raw.clone()
                    } else {
                        render_latex(text, false).unwrap_or_else(|| raw.clone())
                    };
                    result.push_str(&apply_text(&rendered));
                }
            }
        }
        // pi trims a trailing repeated prefix (`markdown.ts:777-780`).
        while !style_prefix.is_empty() && result.ends_with(style_prefix) {
            result.truncate(result.len() - style_prefix.len());
        }
        result
    }

    /// `markdown.ts:465-634`.
    // Long for parity with pi's block renderer (`markdown.ts:465-634`).
    #[allow(clippy::too_many_lines)]
    fn render_block(&self, block: &Block, width: usize, next: Option<&Block>) -> Vec<String> {
        let next_type = next.map(block_type);
        let mut lines: Vec<String> = Vec::new();
        match block {
            Block::Heading { depth, content } => {
                let theme: MarkdownTheme = self.theme.clone();
                let heading_style: Style = if *depth == 1 {
                    Arc::new(move |text: &str| {
                        (theme.heading)(&(theme.bold)(&(theme.underline)(text)))
                    })
                } else {
                    Arc::new(move |text: &str| (theme.heading)(&(theme.bold)(text)))
                };
                let prefix = "#".repeat(*depth);
                let heading_prefix = format!("{prefix} ");
                let style_prefix = Self::style_prefix(&heading_style);
                let heading_text =
                    self.render_inline(content, &style_prefix, heading_style.as_ref());
                // pi shows the `#` prefix from level 3 on (`markdown.ts:496`).
                let styled = if *depth >= 3 {
                    format!("{}{}", heading_style(&heading_prefix), heading_text)
                } else {
                    heading_text
                };
                lines.push(styled);
                if next_type.is_some_and(|kind| kind != "space") {
                    lines.push(String::new());
                }
            }
            Block::Paragraph(content) => {
                let style_prefix = self.default_style_prefix();
                let apply = |text: &str| self.apply_default_style(text);
                lines.push(self.render_inline(content, &style_prefix, &apply));
                if next_type.is_some_and(|kind| kind != "list" && kind != "space") {
                    lines.push(String::new());
                }
            }
            Block::Html(raw) => {
                lines.push(self.apply_default_style(raw.trim()));
            }
            Block::Space => lines.push(String::new()),
            Block::Hr => {
                lines.push((self.theme.hr)(&"─".repeat(width.min(80))));
                if next_type.is_some_and(|kind| kind != "space") {
                    lines.push(String::new());
                }
            }
            Block::Code { lang, text } => {
                let indent = self.theme.code_block_indent.clone();
                lines.push((self.theme.code_block_border)(&format!("```{lang}")));
                if let Some(highlight) = &self.theme.highlight_code {
                    for line in highlight(text, Some(lang.as_str())) {
                        lines.push(format!("{indent}{line}"));
                    }
                } else {
                    for line in text.split('\n') {
                        lines.push(format!("{indent}{}", (self.theme.code_block)(line)));
                    }
                }
                lines.push((self.theme.code_block_border)("```"));
                if next_type.is_some_and(|kind| kind != "space") {
                    lines.push(String::new());
                }
            }
            Block::LatexBlock { text, raw, pending } => {
                // `markdown.ts:614-630`: render display math as Unicode text;
                // if it cannot fit, show the source instead.
                let rendered = if *pending {
                    None
                } else {
                    render_latex(text, true)
                };
                let rendered = rendered
                    .filter(|math| !math.lines().any(|line| visible_width(line) > width))
                    .unwrap_or_else(|| raw.trim().to_string());
                for line in rendered.lines() {
                    lines.push(self.apply_default_style(line));
                }
                if next_type.is_some_and(|kind| kind != "space") {
                    lines.push(String::new());
                }
            }
            Block::BlockQuote(children) => {
                let content_width = width.saturating_sub(2).max(1);
                let quote_style: Style = {
                    let quote_theme: MarkdownTheme = self.theme.clone();
                    Arc::new(move |text: &str| (quote_theme.quote)(&(quote_theme.italic)(text)))
                };
                let quote_style_prefix = Self::style_prefix(&quote_style);
                let mut quote_lines = Vec::new();
                for (position, child) in children.iter().enumerate() {
                    let next = children.get(position + 1);
                    quote_lines.extend(self.render_block(child, content_width, next));
                }
                // pi avoids an extra empty quote line before the outer spacing.
                while quote_lines.last().is_some_and(String::is_empty) {
                    quote_lines.pop();
                }
                for line in quote_lines {
                    // Styles the children closed are re-opened for the quote.
                    let styled = if quote_style_prefix.is_empty() {
                        quote_style(&line)
                    } else {
                        quote_style(
                            &line.replace("\x1b[0m", &format!("\x1b[0m{quote_style_prefix}")),
                        )
                    };
                    for wrapped in wrap_text_with_ansi(&styled, content_width) {
                        lines.push(format!("{}{}", (self.theme.quote_border)("│ "), wrapped));
                    }
                }
                if next_type.is_some_and(|kind| kind != "space") {
                    lines.push(String::new());
                }
            }
            Block::List(list) => {
                lines.extend(self.render_list(list, 0, width));
            }
            Block::Table(table) => {
                lines.extend(self.render_table(table, width, next_type));
            }
        }
        lines
    }

    /// pi's `getDefaultStylePrefix` (`markdown.ts:432-448`): the escapes the
    /// message-level style opens, so an inline token can restore it after its
    /// own reset.
    fn default_style_prefix(&self) -> String {
        let Some(style) = &self.default_text_style else {
            return String::new();
        };
        let sentinel = "\u{0}";
        let mut styled = sentinel.to_string();
        if let Some(color) = &style.color {
            styled = color(&styled);
        }
        for (flag, hook) in [
            (style.bold, &self.theme.bold),
            (style.italic, &self.theme.italic),
            (style.strikethrough, &self.theme.strikethrough),
            (style.underline, &self.theme.underline),
        ] {
            if flag {
                styled = hook(&styled);
            }
        }
        match styled.find(sentinel) {
            Some(index) => styled[..index].to_string(),
            None => String::new(),
        }
    }

    /// `markdown.ts:757-812`.
    fn render_list(&self, list: &List, depth: usize, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let indent = "    ".repeat(depth);
        for (position, item) in list.items.iter().enumerate() {
            let is_last = position == list.items.len() - 1;
            let bullet = if list.ordered {
                item.marker
                    .as_ref()
                    .filter(|_| self.options.preserve_list_markers)
                    .map_or_else(
                        || format!("{}. ", list.start + position),
                        |marker| format!("{marker} "),
                    )
            } else {
                item.marker
                    .as_ref()
                    .filter(|_| self.options.preserve_list_markers)
                    .map_or_else(|| "- ".to_string(), |marker| format!("{marker} "))
            };
            let task_marker = match item.task {
                Some(true) => "[x] ",
                Some(false) => "[ ] ",
                None => "",
            };
            let marker = format!("{bullet}{task_marker}");
            let first_prefix = format!("{indent}{}", (self.theme.list_bullet)(&marker));
            let continuation_prefix = format!("{indent}{}", " ".repeat(visible_width(&marker)));
            let item_width = width.saturating_sub(visible_width(&first_prefix)).max(1);
            let mut rendered_any_line = false;
            for (block_position, block) in item.blocks.iter().enumerate() {
                if let Block::List(nested) = block {
                    lines.extend(self.render_list(nested, depth + 1, width));
                    rendered_any_line = true;
                    continue;
                }
                let next = item.blocks.get(block_position + 1);
                for line in self.render_block(block, item_width, next) {
                    for wrapped in wrap_text_with_ansi(&line, item_width) {
                        let prefix = if rendered_any_line {
                            &continuation_prefix
                        } else {
                            &first_prefix
                        };
                        lines.push(format!("{prefix}{wrapped}"));
                        rendered_any_line = true;
                    }
                }
            }
            if !rendered_any_line {
                lines.push(first_prefix);
            }
            if list.loose && !is_last {
                lines.push(String::new());
            }
        }
        lines
    }

    /// The longest word in `text`, capped at `max_width`. `markdown.ts:815-822`.
    fn longest_word_width(text: &str, max_width: Option<usize>) -> usize {
        let longest = text
            .split_whitespace()
            .map(visible_width)
            .max()
            .unwrap_or(0);
        match max_width {
            Some(max) => longest.min(max),
            None => longest,
        }
    }

    /// `markdown.ts:835-1024`. Column widths are the natural widths when they
    /// fit, otherwise each column grows from its longest word in proportion to
    /// the spare width.
    // Long for parity with pi's `renderTable` (`markdown.ts:835-1024`).
    #[allow(clippy::too_many_lines)]
    fn render_table(
        &self,
        table: &Table,
        available_width: usize,
        next_type: Option<&str>,
    ) -> Vec<String> {
        let mut lines = Vec::new();
        let columns = table.header.len();
        if columns == 0 {
            return lines;
        }
        let border_overhead = 3 * columns + 1;
        let available_for_cells = available_width.saturating_sub(border_overhead);
        if available_for_cells < columns {
            // Too narrow: fall back to the raw markdown.
            lines.extend(wrap_text_with_ansi(&table.raw, available_width));
            if next_type.is_some_and(|kind| kind != "space") {
                lines.push(String::new());
            }
            return lines;
        }
        let max_unbroken_word_width = 30;
        let apply = |text: &str| self.apply_default_style(text);
        let mut natural_widths = vec![0usize; columns];
        let mut min_word_widths = vec![1usize; columns];
        for column in 0..columns {
            let apply = |text: &str| self.apply_default_style(text);
            let text = self.render_inline(&table.header[column], "", &apply);
            natural_widths[column] = visible_width(&text);
            min_word_widths[column] =
                Self::longest_word_width(&text, Some(max_unbroken_word_width)).max(1);
        }
        for row in &table.rows {
            for (column, cell) in row.iter().enumerate() {
                if column >= columns {
                    continue;
                }
                let text = self.render_inline(cell, "", &apply);
                natural_widths[column] = natural_widths[column].max(visible_width(&text));
                min_word_widths[column] = min_word_widths[column]
                    .max(Self::longest_word_width(&text, Some(max_unbroken_word_width)).max(1));
            }
        }

        let mut min_column_widths = min_word_widths.clone();
        let mut min_cells_width: usize = min_column_widths.iter().sum();
        if min_cells_width > available_for_cells {
            min_column_widths = vec![1; columns];
            let remaining = available_for_cells - columns;
            if remaining > 0 {
                let total_weight: usize = min_word_widths
                    .iter()
                    .map(|width| width.saturating_sub(1))
                    .sum();
                let mut growth = vec![0usize; columns];
                if total_weight > 0 {
                    for (column, width) in min_word_widths.iter().enumerate() {
                        let weight = width.saturating_sub(1);
                        growth[column] =
                            (weight * remaining).checked_div(total_weight).unwrap_or(0);
                    }
                }
                for column in 0..columns {
                    min_column_widths[column] += growth[column];
                }
                let allocated: usize = growth.iter().sum();
                let mut leftover = remaining - allocated;
                let mut column = 0;
                while leftover > 0 && column < columns {
                    min_column_widths[column] += 1;
                    leftover -= 1;
                    column += 1;
                }
            }
            min_cells_width = min_column_widths.iter().sum();
        }

        let total_natural_width: usize = natural_widths.iter().sum::<usize>() + border_overhead;
        let column_widths: Vec<usize> = if total_natural_width <= available_width {
            (0..columns)
                .map(|column| natural_widths[column].max(min_column_widths[column]))
                .collect()
        } else {
            let total_grow_potential: usize = (0..columns)
                .map(|column| natural_widths[column].saturating_sub(min_column_widths[column]))
                .sum();
            let extra_width = available_for_cells.saturating_sub(min_cells_width);
            let mut widths: Vec<usize> = (0..columns)
                .map(|column| {
                    let min_width = min_column_widths[column];
                    let min_width_delta = natural_widths[column].saturating_sub(min_width);
                    if total_grow_potential > 0 {
                        min_width
                            + (min_width_delta * extra_width)
                                .checked_div(total_grow_potential)
                                .unwrap_or(0)
                    } else {
                        min_width
                    }
                })
                .collect();
            let mut remaining = available_for_cells - widths.iter().sum::<usize>();
            while remaining > 0 {
                let mut grew = false;
                for column in 0..columns {
                    if remaining == 0 {
                        break;
                    }
                    if widths[column] < natural_widths[column] {
                        widths[column] += 1;
                        remaining -= 1;
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
            widths
        };

        let top: Vec<String> = column_widths
            .iter()
            .map(|width| "─".repeat(*width))
            .collect();
        lines.push(format!("┌─{}─┐", top.join("─┬─")));
        lines.extend(self.render_table_row(&table.header, &column_widths, true));
        let separator: Vec<String> = column_widths
            .iter()
            .map(|width| "─".repeat(*width))
            .collect();
        let separator_line = format!("├─{}─┤", separator.join("─┼─"));
        lines.push(separator_line.clone());
        for (index, row) in table.rows.iter().enumerate() {
            lines.extend(self.render_table_row(row, &column_widths, false));
            if index < table.rows.len() - 1 {
                lines.push(separator_line.clone());
            }
        }
        let bottom: Vec<String> = column_widths
            .iter()
            .map(|width| "─".repeat(*width))
            .collect();
        lines.push(format!("└─{}─┘", bottom.join("─┴─")));
        if next_type.is_some_and(|kind| kind != "space") {
            lines.push(String::new());
        }
        lines
    }

    fn render_table_row(
        &self,
        cells: &[Vec<Inline>],
        column_widths: &[usize],
        header: bool,
    ) -> Vec<String> {
        let wrapped_cells: Vec<Vec<String>> = cells
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                let apply = |text: &str| self.apply_default_style(text);
                let text = self.render_inline(cell, "", &apply);
                wrap_text_with_ansi(&text, column_widths[column].max(1))
            })
            .collect();
        let height = wrapped_cells.iter().map(Vec::len).max().unwrap_or(1);
        let mut lines = Vec::new();
        for line_index in 0..height {
            let mut parts = Vec::new();
            for (column, cell_lines) in wrapped_cells.iter().enumerate() {
                let text = cell_lines.get(line_index).cloned().unwrap_or_default();
                let padded = format!(
                    "{text}{}",
                    " ".repeat(column_widths[column].saturating_sub(visible_width(&text)))
                );
                if header {
                    parts.push((self.theme.bold)(&padded));
                } else {
                    parts.push(padded);
                }
            }
            lines.push(format!("│ {} │", parts.join(" │ ")));
        }
        lines
    }

    /// `markdown.ts:280-388`.
    pub fn render_lines(&self, width: usize) -> Vec<String> {
        if self.text.trim().is_empty() {
            // pi renders zero lines for empty text (`markdown.ts:284-294`).
            return Vec::new();
        }
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        // The transform runs before parsing, with the content width
        // (`markdown.ts:376`).
        let text = match &self.options.transform {
            Some(transform) => transform(&self.text, content_width),
            None => self.text.clone(),
        };
        // Tabs become three spaces, like pi (`markdown.ts:302`).
        let normalized = text.replace('\t', "   ");
        let blocks = parse_blocks(&normalized);

        let mut rendered_lines: Vec<String> = Vec::new();
        for (position, block) in blocks.iter().enumerate() {
            let next = blocks.get(position + 1);
            rendered_lines.extend(self.render_block(block, content_width, next));
        }

        // Wrap, then add the margins and the background. Image lines carry an
        // inline image protocol and must reach the terminal verbatim
        // (`markdown.ts:457-461`).
        let mut wrapped_lines: Vec<String> = Vec::new();
        for line in rendered_lines {
            // Inline tokens can carry newlines from a multi-line paragraph;
            // classify each physical line separately.
            for part in line.split('\n') {
                if crate::terminal_image::is_image_line(part) {
                    wrapped_lines.push(part.to_string());
                } else {
                    wrapped_lines.extend(wrap_text_with_ansi(part, content_width));
                }
            }
        }

        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);
        let background = self
            .default_text_style
            .as_ref()
            .and_then(|style| style.bg_color.clone());
        let mut content_lines: Vec<String> = Vec::new();
        for line in wrapped_lines {
            if crate::terminal_image::is_image_line(&line) {
                content_lines.push(line);
                continue;
            }
            let line_with_margins = format!("{left_margin}{line}{right_margin}");
            if let Some(bg) = &background {
                content_lines.push(apply_background_to_line(
                    &line_with_margins,
                    width,
                    bg.as_ref(),
                ));
            } else {
                let padding = width.saturating_sub(visible_width(&line_with_margins));
                content_lines.push(format!("{line_with_margins}{}", " ".repeat(padding)));
            }
        }

        let empty_line = " ".repeat(width);
        let mut result: Vec<String> = Vec::new();
        for _ in 0..self.padding_y {
            match &background {
                Some(bg) => result.push(apply_background_to_line(&empty_line, width, bg.as_ref())),
                None => result.push(empty_line.clone()),
            }
        }
        result.extend(content_lines);
        for _ in 0..self.padding_y {
            match &background {
                Some(bg) => result.push(apply_background_to_line(&empty_line, width, bg.as_ref())),
                None => result.push(empty_line.clone()),
            }
        }
        result
    }
}

impl Component for Markdown {
    /// `markdown.ts:280-388`, with pi's per-`(text, width)` cache.
    fn render(&self, width: usize) -> Vec<String> {
        let lines = self.render_lines(width);
        if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::strip_terminal_sequences;

    fn plain_at(source: &str, width: usize) -> Vec<String> {
        Markdown::new(
            source,
            0,
            0,
            MarkdownTheme::plain(),
            None,
            MarkdownOptions::default(),
        )
        .render_lines(width)
    }

    fn plain(source: &str) -> Vec<String> {
        plain_at(source, 40)
    }

    /// pi pads every line to the render width; most assertions below care
    /// about the glyphs, so trailing blanks are dropped here and checked
    /// separately by `every_line_is_padded_to_the_render_width`.
    fn plain_text(source: &str, width: usize) -> Vec<String> {
        plain_at(source, width)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .map(|line| line.trim_end().to_string())
            .collect()
    }

    #[test]
    fn empty_text_renders_no_lines() {
        assert!(plain("").is_empty());
        assert!(plain("   \n ").is_empty());
    }

    #[test]
    fn a_paragraph_is_one_line_per_source_line_plus_a_spacer() {
        // No trailing blank: the paragraph is the last token, and pi only adds
        // spacing when there *is* a next token (`markdown.ts:518-521`).
        assert_eq!(plain_text("one\ntwo", 40), ["one", "two"]);
        assert_eq!(plain_text("one\n\ntwo", 40), ["one", "", "two"]);
    }

    #[test]
    fn headings_drop_the_marker_for_levels_one_and_two_only() {
        assert_eq!(plain_text("# Title", 40), ["Title"]);
        assert_eq!(plain_text("## Title", 40), ["Title"]);
        assert_eq!(plain_text("### Title", 40), ["### Title"]);
        assert_eq!(plain_text("# Title\n\nbody", 40), ["Title", "", "body"]);
    }

    #[test]
    fn code_blocks_get_fences_and_a_two_space_indent() {
        assert_eq!(
            plain_text("```rust\nlet x = 1;\n```", 40),
            ["```rust", "  let x = 1;", "```"]
        );
    }

    #[test]
    fn unclosed_code_fences_still_render() {
        assert_eq!(plain_text("```\nabc", 40), ["```", "  abc", "```"]);
    }

    #[test]
    fn lists_use_dash_markers_and_indent_continuations() {
        assert_eq!(plain_text("- one\n- two", 40), ["- one", "- two"]);
        assert_eq!(plain_text("1. one\n2. two", 40), ["1. one", "2. two"]);
    }

    #[test]
    fn nested_lists_indent_by_four_spaces() {
        let lines = plain_text("- one\n  - inner", 40);
        assert_eq!(lines[0], "- one");
        assert_eq!(lines[1], "    - inner");
    }

    #[test]
    fn task_items_render_the_checkbox() {
        assert_eq!(
            plain_text("- [x] done\n- [ ] todo", 40),
            ["- [x] done", "- [ ] todo"]
        );
    }

    #[test]
    fn quotes_get_a_bar_and_a_leading_space() {
        assert_eq!(plain_text("> quoted", 40), ["│ quoted"]);
        assert_eq!(
            plain_text("> quoted\n\nafter", 40),
            ["│ quoted", "", "after"]
        );
    }

    #[test]
    fn rules_are_filled_with_box_drawing() {
        assert_eq!(plain_text("---", 10)[0], "──────────");
    }

    #[test]
    fn tables_get_a_box_with_bold_headers() {
        let theme = MarkdownTheme {
            bold: Arc::new(|text| format!("\x1b[1m{text}\x1b[0m")),
            ..MarkdownTheme::plain()
        };
        let lines = Markdown::new(
            "| a | b |\n| --- | --- |\n| 1 | 2 |",
            0,
            0,
            theme,
            None,
            MarkdownOptions::default(),
        )
        .render_lines(40);
        let texts: Vec<String> = lines
            .iter()
            .map(|line| strip_terminal_sequences(line).trim_end().to_string())
            .collect();
        // Natural widths fit, so every column is exactly one cell wide.
        assert_eq!(texts[0], "┌───┬───┐");
        assert_eq!(texts[1], "│ a │ b │");
        assert_eq!(texts[2], "├───┼───┤");
        assert_eq!(texts[3], "│ 1 │ 2 │");
        assert_eq!(texts[4], "└───┴───┘");
        assert!(lines[1].contains("\x1b[1m"), "the header row is bold");
    }

    #[test]
    fn inline_styles_nest() {
        let theme = MarkdownTheme {
            bold: Arc::new(|text| format!("\x1b[1m{text}\x1b[0m")),
            italic: Arc::new(|text| format!("\x1b[3m{text}\x1b[0m")),
            code: Arc::new(|text| format!("\x1b[2m{text}\x1b[0m")),
            ..MarkdownTheme::plain()
        };
        let lines = Markdown::new(
            "**bold and *italic* `code`**",
            0,
            0,
            theme.clone(),
            None,
            MarkdownOptions::default(),
        )
        .render_lines(40);
        assert_eq!(
            strip_terminal_sequences(&lines[0]).trim_end(),
            "bold and italic code"
        );
        assert!(lines[0].contains("\x1b[1m"), "the strong run is bold");
        assert!(lines[0].contains("\x1b[3m"), "the em run is italic");
        assert!(lines[0].contains("\x1b[2m"), "the code span is dim");
    }

    #[test]
    fn links_print_the_url_when_the_label_differs() {
        // The fallback is capability-dependent; pin "no hyperlinks" so this
        // layout assertion stays deterministic on every host.
        use crate::terminal_image::{
            ImageProtocol, TerminalCapabilities, reset_capabilities_cache, set_capabilities,
        };
        set_capabilities(TerminalCapabilities {
            images: ImageProtocol::Disabled,
            true_color: false,
            hyperlinks: false,
        });
        assert_eq!(
            plain_text("[pi](https://example.com/x)", 60),
            ["pi (https://example.com/x)"]
        );
        assert_eq!(
            plain_text("[https://x.dev](https://x.dev)", 60)[0],
            "https://x.dev"
        );
        reset_capabilities_cache();
    }

    #[test]
    fn strikethrough_needs_a_non_space_after_the_marker() {
        assert_eq!(
            strip_terminal_sequences(&plain_at("~~gone~~", 40)[0]).trim_end(),
            "gone"
        );
        assert_eq!(plain_text("~~ not struck", 40)[0], "~~ not struck");
    }

    #[test]
    fn lines_wrap_to_the_content_width() {
        let lines = plain_text("alpha beta gamma delta epsilon zeta", 12);
        assert!(
            lines.iter().all(|line| line.chars().count() <= 12),
            "{lines:?}"
        );
        assert_eq!(lines.join(" "), "alpha beta gamma delta epsilon zeta");
        assert!(lines.len() > 1, "the text actually wrapped: {lines:?}");
    }

    #[test]
    fn padding_adds_margins_and_blank_lines() {
        let lines = Markdown::new(
            "hi",
            2,
            1,
            MarkdownTheme::plain(),
            None,
            MarkdownOptions::default(),
        )
        .render_lines(10);
        assert_eq!(lines.len(), 3);
        assert_eq!(strip_terminal_sequences(&lines[0]), "          ");
        assert_eq!(strip_terminal_sequences(&lines[1]), "  hi      ");
    }

    #[test]
    fn every_line_is_padded_to_the_render_width() {
        for line in plain_at("some text", 30) {
            assert_eq!(visible_width(&line), 30, "{line:?}");
        }
    }

    #[test]
    fn a_list_after_a_paragraph_does_not_double_space() {
        assert_eq!(plain_text("text\n\n- one", 40), ["text", "- one"]);
    }

    #[test]
    fn preserves_source_list_markers_when_asked() {
        let lines = Markdown::new(
            "1) one",
            0,
            0,
            MarkdownTheme::plain(),
            None,
            MarkdownOptions {
                preserve_list_markers: true,
                ..MarkdownOptions::default()
            },
        )
        .render_lines(40);
        assert_eq!(strip_terminal_sequences(&lines[0]).trim_end(), "1) one");
    }

    #[test]
    fn ordered_lists_number_from_the_first_marker() {
        assert_eq!(plain_text("3. three\n4. four", 40), ["3. three", "4. four"]);
    }

    /// Regression: the list parser used to return an *empty* list for an
    /// ordered marker it then refused to consume, and the block loop appended
    /// it forever, growing without bound. Every block parser must consume at
    /// least one line per iteration.
    #[test]
    fn every_marker_shaped_source_terminates() {
        for source in [
            "1) one",
            "1. one",
            "- one",
            "* one",
            "+ one",
            "*bold*",
            "-",
            "- \n- ",
            "1)",
            "> ",
            "```",
            "```rust\n",
            "   ",
            "- a\n\n- b",
            "1. a\n  1. b",
        ] {
            let rendered = Markdown::new(
                source,
                0,
                0,
                MarkdownTheme::plain(),
                None,
                MarkdownOptions::default(),
            )
            .render_lines(20);
            assert!(
                rendered.len() < 200,
                "{source:?} produced {} lines",
                rendered.len()
            );
        }
    }

    #[test]
    fn a_bullet_needs_whitespace_after_it() {
        // `**bold**` is emphasis, not a list item: the plain theme renders the
        // strong run as its own text and no bullet appears.
        assert_eq!(plain_text("**bold**", 20)[0], "bold");
    }

    // --- markdown extras: LaTeX, highlighting, links, image lines -----------

    #[test]
    fn inline_latex_renders_unicode_math() {
        assert_eq!(
            plain_text("Euler: $x^2 + \\alpha$ done", 60),
            ["Euler: x² + α done"]
        );
        assert_eq!(plain_text("cost is $5 and $6", 60), ["cost is $5 and $6"]);
    }

    #[test]
    fn display_latex_block_renders_math_and_falls_back_to_source_when_unsupported() {
        assert_eq!(plain_text("$$\nx^2 + y^2\n$$", 40), ["x² + y²"]);
        // An unsupported expression falls back to the whole raw block.
        assert_eq!(
            plain_text("$$\n\\begin{matrix}a\\end{matrix}\n$$", 60),
            ["$$", "\\begin{matrix}a\\end{matrix}", "$$"]
        );
    }

    #[test]
    fn code_blocks_are_syntax_highlighted_with_sgr_segments() {
        let theme = MarkdownTheme {
            highlight_code: Some(Arc::new(|code: &str, lang: Option<&str>| {
                markdown_extras::highlight_segments(code, lang)
                    .into_iter()
                    .map(|segments| {
                        segments
                            .into_iter()
                            .map(|(kind, text)| {
                                if kind == markdown_extras::SyntaxKind::Keyword {
                                    format!("\x1b[35m{text}\x1b[0m")
                                } else {
                                    text
                                }
                            })
                            .collect::<String>()
                    })
                    .collect()
            })),
            ..MarkdownTheme::plain()
        };
        let lines = Markdown::new(
            "```rust\nfn main() {}\n```",
            0,
            0,
            theme,
            None,
            MarkdownOptions::default(),
        )
        .render_lines(40);
        assert!(lines[1].contains("\x1b[35mfn\x1b[0m"));
        assert_eq!(
            strip_terminal_sequences(&lines[1]).trim_end(),
            "  fn main() {}"
        );
    }

    #[test]
    fn links_use_osc8_when_the_terminal_supports_hyperlinks() {
        use crate::terminal_image::{
            ImageProtocol, TerminalCapabilities, reset_capabilities_cache, set_capabilities,
        };
        set_capabilities(TerminalCapabilities {
            images: ImageProtocol::Disabled,
            true_color: false,
            hyperlinks: true,
        });
        let lines = plain_at("[label](https://example.com/x)", 60);
        assert!(lines[0].contains("\x1b]8;;https://example.com/x\x1b\\"));
        assert!(!lines[0].contains("(https://example.com/x)"));
        reset_capabilities_cache();

        set_capabilities(TerminalCapabilities {
            images: ImageProtocol::Disabled,
            true_color: false,
            hyperlinks: false,
        });
        assert_eq!(
            strip_terminal_sequences(&plain_at("[label](https://example.com/x)", 60)[0]).trim_end(),
            "label (https://example.com/x)"
        );
        reset_capabilities_cache();
    }

    #[test]
    fn image_lines_pass_through_unwrapped_and_unpadded() {
        let image = "\x1b]1337;File=inline=1:AAAA\x07";
        let source = format!("before\n{image}\nafter");
        let lines = plain_at(&source, 10);
        assert!(
            lines.iter().any(|line| line == image),
            "the image line must reach the terminal verbatim: {lines:?}"
        );
        assert_eq!(strip_terminal_sequences(&lines[0]).trim_end(), "before");
    }
}
