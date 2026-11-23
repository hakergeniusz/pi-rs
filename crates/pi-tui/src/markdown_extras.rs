//! Markdown extras: the LaTeX tokenizer/renderer and the code highlighter.
//!
//! Ground truth: `packages/tui/src/components/markdown.ts:8-150` (the LaTeX
//! tokenizer extension), `packages/tui/src/latex.ts:3-1506` (the Unicode math
//! renderer), `packages/coding-agent/src/modes/interactive/theme/theme.ts:
//! 887-954` (the `highlightCode` token-to-theme mapping).
//!
//! This is a faithful **subset** of upstream's 1,506-line math renderer. The
//! tokenizer is ported branch for branch; the renderer implements upstream's
//! symbol tables, `formatScript`/`formatFraction`/`formatRoot`, the spacing
//! commands, named operators, `\left`/`\right`, simple accents, and stacked
//! fractions/operator limits for display math. Unsupported LaTeX commands make
//! [`render_latex`] return `None`, which is exactly upstream's signal to show
//! the raw source instead. Ported commands also include `\overset`/
//! `\underset`, `\mod`/`\bmod`, and seven combining accents. Not ported:
//! matrices/environments, `\not`, `\mathbb`, `\binom`, `\boxed`, `\pmod`,
//! font-switching wrappers (ignored, as upstream ignores the switches), and
//! the 2-D `script` layout (display scripts use Unicode/inline notation).
//!
//! The highlighter is a dependency-free lexer producing upstream's token
//! kinds; [`crate::theme::Theme::markdown_theme`] maps them onto the theme's
//! `syntax*` colours. Upstream delegates to highlight.js; this subset covers
//! json, rust, ts/js and shell/python (plus common aliases).

use crate::ansi::visible_width;

// ===========================================================================
// LaTeX tokenizer (`markdown.ts:37-150`)
// ===========================================================================

/// Upstream's `LatexToken.type` (`markdown.ts:37-41`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatexKind {
    /// `$…$`, `$$…$$` on one line, `\(…\)`, `\[…\]` on one line.
    Inline,
    /// A `$$…$$` or `\[…\]` block spanning one or more lines.
    Block,
}

/// A LaTeX token, mirroring `LatexToken` (`markdown.ts:37-41`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatexToken {
    pub kind: LatexKind,
    /// The exact source consumed, including the delimiters.
    pub raw: String,
    /// The math body, without delimiters.
    pub text: String,
    /// A streamed, still-open expression (`markdown.ts:40`).
    pub pending: bool,
}

/// `isEscaped` (`markdown.ts:43-49`): an odd number of preceding backslashes.
fn is_escaped(chars: &[char], index: usize) -> bool {
    let mut backslashes = 0usize;
    let mut position = index;
    while position > 0 && chars[position - 1] == '\\' {
        backslashes += 1;
        position -= 1;
    }
    backslashes % 2 == 1
}

/// `findClosingDelimiter` (`markdown.ts:51-57`): first unescaped closing marker.
fn find_closing_delimiter(chars: &[char], closing: &[char], start: usize) -> Option<usize> {
    let mut index = start;
    while index + closing.len() <= chars.len() {
        if chars[index..].starts_with(closing) {
            if !is_escaped(chars, index) {
                return Some(index);
            }
            index += closing.len();
        } else {
            index += 1;
        }
    }
    None
}

/// `looksLikePendingDollarMath` (`markdown.ts:59-61`): the regex
/// `\\[A-Za-z]+|[_^=+*/<>()[\]|±≤≥≠≈∈→⇒∞∫∑√-]`, written without a regex crate.
fn looks_like_pending_dollar_math(source: &str) -> bool {
    let chars: Vec<char> = source.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        let c = chars[index];
        if c == '\\' && chars.get(index + 1).is_some_and(char::is_ascii_alphabetic) {
            return true;
        }
        if matches!(
            c,
            '_' | '^'
                | '='
                | '+'
                | '*'
                | '/'
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '|'
                | '±'
                | '≤'
                | '≥'
                | '≠'
                | '≈'
                | '∈'
                | '→'
                | '⇒'
                | '∞'
                | '∫'
                | '∑'
                | '√'
                | '-'
        ) {
            return true;
        }
        index += 1;
    }
    false
}

/// The `TABLE_DELIMITER_ROW_REGEX` of `markdown.ts:112`.
fn is_table_delimiter_row(line: &str) -> bool {
    let line = line.trim_start_matches(' ').trim_end();
    if line.len() > 3 && line.starts_with("   ") {
        return false;
    }
    let inner = line.strip_prefix('|').unwrap_or(line);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    let mut saw_dash = false;
    for cell in inner.split('|') {
        let cell = cell.trim_matches(|c| c == ' ' || c == '\t');
        let cell = cell.strip_prefix(':').unwrap_or(cell);
        let cell = cell.strip_suffix(':').unwrap_or(cell);
        // Upstream requires `-+` in each cell: only dashes are allowed and at
        // least one is mandatory.
        if cell.is_empty() || cell.chars().any(|c| c != '-') {
            return false;
        }
        saw_dash = true;
    }
    saw_dash
}

/// `containsTableStart` (`markdown.ts:117-126`).
fn contains_table_start(source: &str) -> bool {
    let lines: Vec<&str> = source.split('\n').collect();
    for pair in lines.windows(2) {
        if (pair[1].contains('|') || pair[0].contains('|')) && is_table_delimiter_row(pair[1]) {
            return true;
        }
    }
    false
}

/// `LATEX_MARKDOWN_EXTENSIONS[inline].start` (`markdown.ts:163-172`).
pub fn find_inline_latex_start(source: &str) -> Option<usize> {
    [source.find('$'), source.find("\\("), source.find("\\[")]
        .into_iter()
        .flatten()
        .min()
}

/// `tokenizeInlineLatex` (`markdown.ts:63-110`). `source` starts at the `$`,
/// `\(` or `\[` found by [`find_inline_latex_start`].
pub fn tokenize_inline_latex(source: &str) -> Option<LatexToken> {
    let chars: Vec<char> = source.chars().collect();
    let (opening, closing): (&str, &str) = if source.starts_with("$$") {
        ("$$", "$$")
    } else if source.starts_with("\\(") {
        ("\\(", "\\)")
    } else if source.starts_with("\\[") {
        ("\\[", "\\]")
    } else if source.starts_with('$') && !source.starts_with("$ ") && !source.starts_with("$\t") {
        ("$", "$")
    } else {
        return None;
    };
    let open_chars: Vec<char> = opening.chars().collect();
    let close_chars: Vec<char> = closing.chars().collect();
    let closing_index = find_closing_delimiter(&chars, &close_chars, open_chars.len());

    if let Some(close) = closing_index {
        let inner: String = chars[open_chars.len()..close].iter().collect();
        if opening == "$" {
            let after: String = chars[close + close_chars.len()..].iter().collect();
            let inner_ends_space = inner.chars().last().is_some_and(char::is_whitespace);
            let after_starts_digit = after.chars().next().is_some_and(|c| c.is_ascii_digit());
            // `^[A-Z_][A-Z0-9_]*(?:[^A-Za-z0-9_\s])?$` followed by an identifier.
            let uppercase_symbol = {
                let mut cs = inner.chars();
                cs.next()
                    .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
                    && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            };
            let after_identifier = {
                let mut cs = after.chars();
                cs.next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
            };
            if inner_ends_space
                || after_starts_digit
                || (uppercase_symbol && after_identifier)
                || inner.contains('`')
            {
                return None;
            }
        }
    } else {
        let pending_source: String = chars[open_chars.len()..].iter().collect();
        return opening
            .starts_with('\\')
            .then(|| LatexToken {
                kind: LatexKind::Inline,
                raw: source.to_string(),
                text: pending_source.clone(),
                pending: true,
            })
            .or_else(|| {
                looks_like_pending_dollar_math(&pending_source).then(|| LatexToken {
                    kind: LatexKind::Inline,
                    raw: source.to_string(),
                    text: pending_source,
                    pending: true,
                })
            });
    }

    let close = closing_index?;
    let text: String = chars[open_chars.len()..close].iter().collect();
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    let raw: String = chars[..close + close_chars.len()].iter().collect();
    Some(LatexToken {
        kind: LatexKind::Inline,
        raw,
        text,
        pending: false,
    })
}

/// Match `^ {0,3}<opener>[ \t]*(?:\n)?` at the start of a block source.
/// Returns the body's starting byte offset and the closing delimiter.
fn block_open<'a>(source: &'a str, opener: &str) -> Option<(usize, &'a str)> {
    let spaces = source.chars().take_while(|c| *c == ' ').count();
    if spaces > 3 {
        return None;
    }
    let rest = &source[spaces..];
    let body = rest.strip_prefix(opener)?;
    let body = body.trim_start_matches([' ', '\t']);
    let body = body.strip_prefix('\n').unwrap_or(body);
    let closer = if opener == "$$" { "$$" } else { "\\]" };
    Some((source.len() - body.len(), closer))
}

/// `tokenizeBlockLatex` (`markdown.ts:128-150`). `source` is the remaining
/// markdown source at a line boundary (up to three leading spaces allowed).
pub fn tokenize_block_latex(source: &str) -> Option<LatexToken> {
    for opener in ["$$", "\\["] {
        let Some((body_start, closer)) = block_open(source, opener) else {
            continue;
        };
        let body = &source[body_start..];
        if let Some(end) = body.find(closer) {
            let text = &body[..end];
            if !text.is_empty() && !contains_table_start(text) {
                let after = &body[end + closer.len()..];
                let after = after.trim_start_matches([' ', '\t']);
                if after.is_empty() || after.starts_with('\n') {
                    let consumed = end
                        + closer.len()
                        + (after
                            .len()
                            .saturating_sub(after.trim_start_matches('\n').len()));
                    return Some(LatexToken {
                        kind: LatexKind::Block,
                        raw: source[..body_start + consumed].to_string(),
                        text: text.trim().to_string(),
                        pending: false,
                    });
                }
            }
        }
        // Pending block: an unclosed opener is kept as source while streaming.
        let looks_math = opener != "$$" || looks_like_pending_dollar_math(body);
        if looks_math && !body.is_empty() && !contains_table_start(body) {
            return Some(LatexToken {
                kind: LatexKind::Block,
                raw: source.to_string(),
                text: body.to_string(),
                pending: true,
            });
        }
    }
    None
}

/// How many source lines a block token consumes (its `raw` ends at a newline).
pub fn block_token_line_count(token: &LatexToken) -> usize {
    token.raw.lines().count().max(1)
}

// ===========================================================================
// Minimal LaTeX renderer (`latex.ts`)
// ===========================================================================

const OPENER: char = '\u{f0000}';
const CLOSER: char = '\u{f0001}';
const NAME_START: char = '\u{f0004}';
const NAME_END: char = '\u{f0005}';
const PROTECTED_SPACE: char = '\u{f0002}';
const NEGATIVE_SPACE: char = '\0';

/// Upstream's `SYMBOLS` (`latex.ts:3-228`), restricted to the commands this
/// subset translates. Every glyph is copied verbatim from upstream.
// Keep every alias as a separate arm, like upstream's lookup table.
#[allow(clippy::match_same_arms, clippy::too_many_lines)]
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "varkappa" => "ϰ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "oslash" => "⊘",
        "odot" => "⊙",
        "bigcirc" => "○",
        "dagger" => "†",
        "ddagger" => "‡",
        "cap" => "∩",
        "cup" => "∪",
        "bigcap" => "⋂",
        "bigcup" => "⋃",
        "bigwedge" => "⋀",
        "bigvee" => "⋁",
        "setminus" => "∖",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "prec" => "≺",
        "preceq" => "≼",
        "succ" => "≻",
        "succeq" => "≽",
        "ll" => "≪",
        "gg" => "≫",
        "le" => "≤",
        "leq" => "≤",
        "leqslant" => "≤",
        "ge" => "≥",
        "geq" => "≥",
        "geqslant" => "≥",
        "ne" => "≠",
        "neq" => "≠",
        "equiv" => "≡",
        "approx" => "≈",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "propto" => "∝",
        "parallel" => "∥",
        "perp" => "⊥",
        "mid" => "∣",
        "vdash" => "⊢",
        "models" => "⊨",
        "forall" => "∀",
        "exists" => "∃",
        "neg" => "¬",
        "land" => "∧",
        "wedge" => "∧",
        "lor" => "∨",
        "vee" => "∨",
        "to" => "→",
        "rightarrow" => "→",
        "longrightarrow" => "→",
        "leftarrow" => "←",
        "longleftarrow" => "←",
        "gets" => "←",
        "leftrightarrow" => "↔",
        "longleftrightarrow" => "↔",
        "mapsto" => "↦",
        "longmapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "partial" => "∂",
        "nabla" => "∇",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "infty" => "∞",
        "emptyset" => "∅",
        "angle" => "∠",
        "therefore" => "∴",
        "because" => "∵",
        "aleph" => "ℵ",
        "top" => "⊤",
        "bot" => "⊥",
        "triangle" => "△",
        "square" => "□",
        "prime" => "′",
        "ldots" => "…",
        "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "ell" => "ℓ",
        "hbar" => "ℏ",
        "Im" => "ℑ",
        "Re" => "ℜ",
        "langle" => "⟨",
        "rangle" => "⟩",
        "vert" => "|",
        "Vert" => "‖",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "colon" => ":",
        _ => return None,
    })
}

fn superscript(c: char) -> Option<&'static str> {
    Some(match c {
        '0' => "⁰",
        '1' => "¹",
        '2' => "²",
        '3' => "³",
        '4' => "⁴",
        '5' => "⁵",
        '6' => "⁶",
        '7' => "⁷",
        '8' => "⁸",
        '9' => "⁹",
        '+' => "⁺",
        '-' => "⁻",
        '=' => "⁼",
        '(' => "⁽",
        ')' => "⁾",
        'a' => "ᵃ",
        'b' => "ᵇ",
        'c' => "ᶜ",
        'd' => "ᵈ",
        'e' => "ᵉ",
        'f' => "ᶠ",
        'g' => "ᵍ",
        'h' => "ʰ",
        'i' => "ⁱ",
        'j' => "ʲ",
        'k' => "ᵏ",
        'l' => "ˡ",
        'm' => "ᵐ",
        'n' => "ⁿ",
        'o' => "ᵒ",
        'p' => "ᵖ",
        'r' => "ʳ",
        's' => "ˢ",
        't' => "ᵗ",
        'u' => "ᵘ",
        'v' => "ᵛ",
        'w' => "ʷ",
        'x' => "ˣ",
        'y' => "ʸ",
        'z' => "ᶻ",
        _ => return None,
    })
}

fn subscript(c: char) -> Option<&'static str> {
    Some(match c {
        '0' => "₀",
        '1' => "₁",
        '2' => "₂",
        '3' => "₃",
        '4' => "₄",
        '5' => "₅",
        '6' => "₆",
        '7' => "₇",
        '8' => "₈",
        '9' => "₉",
        '+' => "₊",
        '-' => "₋",
        '=' => "₌",
        '(' => "₍",
        ')' => "₎",
        'a' => "ₐ",
        'e' => "ₑ",
        'h' => "ₕ",
        'i' => "ᵢ",
        'j' => "ⱼ",
        'k' => "ₖ",
        'l' => "ₗ",
        'm' => "ₘ",
        'n' => "ₙ",
        'o' => "ₒ",
        'p' => "ₚ",
        'r' => "ᵣ",
        's' => "ₛ",
        't' => "ₜ",
        'u' => "ᵤ",
        'v' => "ᵥ",
        'x' => "ₓ",
        _ => return None,
    })
}

/// `normalizeScriptValue` (`latex.ts:614-616`).
fn normalize_script_value(value: &str) -> String {
    let trimmed = value.trim();
    let mut out = String::new();
    let mut previous_space = false;
    for c in trimmed.chars() {
        if c.is_whitespace() {
            previous_space = true;
        } else {
            if previous_space && matches!(c, '=' | '+' | '-') {
                // Upstream removes the space around these operators.
            } else if previous_space {
                out.push(' ');
            }
            out.push(c);
            previous_space = false;
        }
    }
    out
}

/// `formatScript` (`latex.ts:618-634`): prefer Unicode sub/superscripts, then
/// a bare one-character/letter script, then parentheses.
fn format_script(value: &str, kind: ScriptKind) -> String {
    let value = normalize_script_value(value);
    let convert = |c: char| match kind {
        ScriptKind::Sub => subscript(c),
        ScriptKind::Sup => superscript(c),
    };
    if !value.is_empty() && value.chars().all(|c| convert(c).is_some()) {
        return value.chars().filter_map(convert).collect();
    }
    let prefix = match kind {
        ScriptKind::Sub => '_',
        ScriptKind::Sup => '^',
    };
    let count = value.chars().count();
    if count == 1
        || (matches!(kind, ScriptKind::Sub)
            && !value.is_empty()
            && value.chars().all(|c| c.is_ascii_alphabetic()))
    {
        format!("{prefix}{value}")
    } else {
        format!("{prefix}({value})")
    }
}

#[derive(Debug, Clone, Copy)]
enum ScriptKind {
    Sub,
    Sup,
}

/// `formatFraction` (`latex.ts:636-642`).
fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = numerator.trim();
    let denominator = denominator.trim();
    let simple_numerator =
        !numerator.is_empty() && numerator.chars().all(|c| c.is_alphanumeric() || c == '.');
    let simple_denominator = (!denominator.is_empty()
        && denominator.chars().all(|c| c.is_ascii_digit() || c == '.'))
        || denominator.chars().count() == 1;
    let n = if simple_numerator {
        numerator.to_string()
    } else {
        format!("({numerator})")
    };
    let d = if simple_denominator {
        denominator.to_string()
    } else {
        format!("({denominator})")
    };
    format!("{n}/{d}")
}

/// `formatRoot` (`latex.ts:644-647`).
fn format_root(value: &str, symbol: &str) -> String {
    let value = value.trim();
    if !value.is_empty() && value.chars().all(|c| c.is_alphanumeric() || c == '.') {
        format!("{symbol}{value}")
    } else {
        format!("{symbol}({value})")
    }
}

fn spacing_command(name: &str) -> bool {
    matches!(
        name,
        "," | ":"
            | ";"
            | " "
            | ">"
            | "enspace"
            | "enskip"
            | "medspace"
            | "quad"
            | "qquad"
            | "thickspace"
            | "thinspace"
    )
}

fn negative_spacing_command(name: &str) -> bool {
    matches!(name, "!" | "negmedspace" | "negthickspace" | "negthinspace")
}

fn ignored_command(name: &str) -> bool {
    matches!(
        name,
        "displaystyle" | "limits" | "nolimits" | "scriptstyle" | "scriptscriptstyle" | "textstyle"
    )
}

fn plain_wrapper(name: &str) -> bool {
    matches!(
        name,
        "emph"
            | "mathcal"
            | "mathbf"
            | "mathfrak"
            | "mathit"
            | "mathrm"
            | "mathnormal"
            | "mathscr"
            | "mathsf"
            | "mathtt"
            | "mathup"
            | "mbox"
            | "text"
            | "textbf"
            | "textit"
            | "textmd"
            | "textnormal"
            | "textrm"
            | "textsc"
            | "textsf"
            | "textsl"
            | "texttt"
            | "textup"
    )
}

fn named_operator(name: &str) -> bool {
    matches!(
        name,
        "arccos"
            | "arcsin"
            | "arctan"
            | "arg"
            | "cos"
            | "cosh"
            | "cot"
            | "coth"
            | "csc"
            | "deg"
            | "det"
            | "dim"
            | "exp"
            | "gcd"
            | "hom"
            | "inf"
            | "ker"
            | "lg"
            | "lim"
            | "liminf"
            | "limsup"
            | "ln"
            | "log"
            | "max"
            | "min"
            | "Pr"
            | "sec"
            | "sin"
            | "sinh"
            | "sup"
            | "tan"
            | "tanh"
    )
}

fn limit_operator(name: &str) -> bool {
    matches!(
        name,
        "argmax"
            | "argmin"
            | "inf"
            | "injlim"
            | "lim"
            | "liminf"
            | "limsup"
            | "max"
            | "min"
            | "projlim"
            | "sup"
    )
}

fn relation_command(name: &str) -> bool {
    matches!(
        name,
        "approx"
            | "equiv"
            | "ge"
            | "geq"
            | "gg"
            | "in"
            | "implies"
            | "iff"
            | "le"
            | "leq"
            | "ll"
            | "ne"
            | "neq"
            | "ni"
            | "rightarrow"
            | "leftarrow"
            | "leftrightarrow"
            | "subset"
            | "subseteq"
            | "supset"
            | "supseteq"
            | "to"
            | "Rightarrow"
            | "Leftarrow"
            | "Leftrightarrow"
            | "mapsto"
            | "sim"
            | "simeq"
            | "perp"
            | "mid"
            | "parallel"
            | "propto"
            | "vdash"
            | "models"
    )
}

/// A 2-D fragment of display math (`latex.ts:667-735`).
#[derive(Debug, Clone)]
struct Layout {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

fn visible(text: &str) -> usize {
    visible_width(text)
}

fn pad_layout(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible(line));
    let left = if centered { padding / 2 } else { 0 };
    format!("{}{}{}", " ".repeat(left), line, " ".repeat(padding - left))
}

fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout {
            lines: vec![String::new()],
            width: 0,
            baseline: 0,
        };
    }
    let baseline = layouts.iter().map(|l| l.baseline).max().unwrap_or(0);
    let below = layouts
        .iter()
        .map(|l| l.lines.len().saturating_sub(l.baseline + 1))
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for row in 0..=baseline + below {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row + layout.baseline;
            if source_row >= baseline && source_row - baseline < layout.lines.len() {
                line.push_str(&pad_layout(
                    &layout.lines[source_row - baseline],
                    layout.width,
                    false,
                ));
            } else {
                line.push_str(&" ".repeat(layout.width));
            }
        }
        lines.push(line.trim_end().to_string());
    }
    Layout {
        width: layouts.iter().map(|l| l.width).sum(),
        lines,
        baseline,
    }
}

#[derive(Debug, Clone)]
enum LayoutNode {
    Fraction {
        numerator: String,
        denominator: String,
    },
    Operator {
        operator: String,
        lower: Option<String>,
        upper: Option<String>,
    },
}

/// `renderLayout` (`latex.ts:737-836`) for the subset's nodes.
fn render_layout(source: &str, nodes: &[LayoutNode]) -> Layout {
    let mut rendered: Vec<String> = Vec::new();
    let mut first_baseline = 0usize;
    for source_line in source.split('\n') {
        let mut chars: Vec<char> = source_line.chars().collect();
        let mut layouts: Vec<Layout> = Vec::new();
        let mut position = 0usize;
        while position < chars.len() {
            if chars[position] == OPENER {
                let mut end = position + 1;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                let index: String = chars[position + 1..end].iter().collect();
                let next = if end < chars.len() && chars[end] == CLOSER {
                    end + 1
                } else {
                    end
                };
                let node = index.parse::<usize>().ok().and_then(|i| nodes.get(i));
                if let Some(node) = node {
                    if position > 0 {
                        let text: String = chars[..position].iter().collect();
                        layouts.push(Layout {
                            width: visible(&text),
                            lines: vec![text],
                            baseline: 0,
                        });
                    }
                    let layout = match node {
                        LayoutNode::Fraction {
                            numerator,
                            denominator,
                        } => {
                            let n = render_layout(numerator, nodes);
                            let d = render_layout(denominator, nodes);
                            let content = n.width.max(d.width).max(1);
                            let width = content + 2;
                            let mut lines: Vec<String> = n
                                .lines
                                .iter()
                                .map(|line| pad_layout(line, width, true))
                                .collect();
                            lines.push(format!(" ─{} ", "─".repeat(content - 1)));
                            lines.extend(d.lines.iter().map(|line| pad_layout(line, width, true)));
                            Layout {
                                lines,
                                width,
                                baseline: n.lines.len(),
                            }
                        }
                        LayoutNode::Operator {
                            operator,
                            lower,
                            upper,
                        } => {
                            let content = visible(operator)
                                .max(lower.as_deref().map_or(0, visible))
                                .max(upper.as_deref().map_or(0, visible));
                            let mut lines = Vec::new();
                            if let Some(upper) = upper {
                                lines.push(format!("{} ", pad_layout(upper, content, true)));
                            }
                            lines.push(format!("{} ", pad_layout(operator, content, true)));
                            if let Some(lower) = lower {
                                lines.push(format!("{} ", pad_layout(lower, content, true)));
                            }
                            Layout {
                                width: content + 1,
                                lines,
                                baseline: usize::from(upper.is_some()),
                            }
                        }
                    };
                    layouts.push(layout);
                    chars.drain(..next);
                    position = 0;
                    continue;
                }
            }
            position += 1;
        }
        if !chars.is_empty() {
            let text: String = chars.iter().collect();
            layouts.push(Layout {
                width: visible(&text),
                lines: vec![text],
                baseline: 0,
            });
        }
        let line_layout = join_layouts(&layouts);
        if rendered.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered.extend(line_layout.lines);
    }
    Layout {
        width: rendered.iter().map(|line| visible(line)).max().unwrap_or(0),
        lines: rendered,
        baseline: first_baseline,
    }
}

/// `normalizeOutput` (`latex.ts:654-665`), including the named-operator
/// spacing sentinels.
fn normalize_output(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut result: Vec<char> = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        let c = chars[index];
        if c == NAME_START {
            if result
                .last()
                .is_some_and(|p| p.is_alphanumeric() || matches!(*p, ')' | ']' | '}' | CLOSER))
            {
                result.push(' ');
            }
            index += 1;
            continue;
        }
        if c == NAME_END {
            index += 1;
            if chars
                .get(index)
                .is_some_and(|n| n.is_alphanumeric() || matches!(*n, '√' | OPENER))
            {
                result.push(' ');
            }
            continue;
        }
        result.push(c);
        index += 1;
    }
    let joined: String = result.into_iter().collect();
    joined
        .split('\n')
        .map(|line| {
            let mut out = String::new();
            let mut space = false;
            for c in line.chars() {
                if c == ' ' || c == '\t' {
                    space = !out.is_empty();
                } else {
                    if space {
                        out.push(' ');
                        space = false;
                    }
                    out.push(c);
                }
            }
            out.trim().to_string()
        })
        .collect::<Vec<String>>()
        .join("\n")
        .trim()
        .to_string()
}

/// A subset of upstream's `LatexParser` (`latex.ts:838-1477`).
struct LatexParser {
    source: Vec<char>,
    position: usize,
    display: bool,
    stack_fractions: bool,
    nodes: Vec<LayoutNode>,
}

impl LatexParser {
    fn new(source: &str, display: bool) -> Self {
        Self {
            source: source.chars().collect(),
            position: 0,
            display,
            stack_fractions: true,
            nodes: Vec::new(),
        }
    }

    fn render(mut self) -> Option<String> {
        let rendered = self.parse_sequence(None)?;
        if self.position != self.source.len() {
            return None;
        }
        if self.nodes.is_empty() {
            return Some(rendered.replace(PROTECTED_SPACE, " "));
        }
        let layout = render_layout(&rendered, &self.nodes);
        let indentation = layout
            .lines
            .iter()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.len() - line.trim_start().len())
            .min()
            .unwrap_or(0);
        let text = layout
            .lines
            .iter()
            .map(|line| line[indentation.min(line.len())..].trim_end())
            .collect::<Vec<&str>>()
            .join("\n")
            .trim_end()
            .replace(PROTECTED_SPACE, " ");
        Some(text)
    }

    fn parse_sequence(&mut self, end: Option<char>) -> Option<String> {
        let mut result = String::new();
        while self.position < self.source.len() {
            let c = self.source[self.position];
            if end == Some(c) {
                self.position += 1;
                return Some(result);
            }
            if c == '}' {
                return None;
            }
            if c == '{' {
                self.position += 1;
                result.push_str(&self.parse_sequence(Some('}'))?);
                continue;
            }
            if c == '\\' {
                let command = self.parse_command()?;
                if command == NEGATIVE_SPACE.to_string() {
                    result = result.trim_end().to_string();
                    if result.ends_with(NAME_END) {
                        result.pop();
                    }
                } else {
                    result.push_str(&command);
                }
                continue;
            }
            if c == '^' || c == '_' {
                self.position += 1;
                result = result.trim_end().to_string();
                let script = self.parse_scripts(if c == '_' {
                    ScriptKind::Sub
                } else {
                    ScriptKind::Sup
                })?;
                result.push_str(&script);
                continue;
            }
            if c.is_whitespace() {
                self.position += 1;
                while self.position < self.source.len()
                    && self.source[self.position].is_whitespace()
                {
                    self.position += 1;
                }
                result.push(' ');
                continue;
            }
            if matches!(c, '=' | '<' | '>') {
                result = format!("{} {c} ", result.trim_end());
                self.position += 1;
                continue;
            }
            if c == '~' {
                self.position += 1;
                result.push(PROTECTED_SPACE);
                continue;
            }
            result.push(c);
            self.position += 1;
        }
        if end.is_some() {
            return None;
        }
        Some(result)
    }

    /// `parseScripts` (`latex.ts:949-1005`), simplified to the inline Unicode
    /// notation (no 2-D script layout).
    fn parse_scripts(&mut self, initial: ScriptKind) -> Option<String> {
        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        let mut order = Vec::new();
        let mut kind = initial;
        loop {
            let value = self.parse_required_argument(false)?;
            match kind {
                ScriptKind::Sub => lower = Some(value),
                ScriptKind::Sup => upper = Some(value),
            }
            order.push(kind);
            let mut lookahead = self.position;
            while lookahead < self.source.len() && self.source[lookahead].is_whitespace() {
                lookahead += 1;
            }
            let next = self.source.get(lookahead).copied();
            let next_kind = match next {
                Some('_') if !matches!(initial, ScriptKind::Sub) => Some(ScriptKind::Sub),
                Some('^') if !matches!(initial, ScriptKind::Sup) => Some(ScriptKind::Sup),
                _ => None,
            };
            match next_kind {
                Some(k) => {
                    self.position = lookahead + 1;
                    kind = k;
                }
                None => break,
            }
        }
        Some(
            order
                .into_iter()
                .map(|k| match k {
                    ScriptKind::Sub => format_script(lower.as_deref().unwrap_or(""), k),
                    ScriptKind::Sup => format_script(upper.as_deref().unwrap_or(""), k),
                })
                .collect(),
        )
    }

    /// `parseCommand` (`latex.ts:1014-1192`), minus the constructs listed in
    /// the module docs (matrices, `\not`, `\mathbb`, `\boxed`, …).
    // Mirrors the upstream command switch; split when extending.
    #[allow(clippy::too_many_lines)]
    fn parse_command(&mut self) -> Option<String> {
        self.position += 1;
        let first = *self.source.get(self.position)?;
        if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r' && self.source.get(self.position) == Some(&'\n') {
                self.position += 1;
            }
            return Some(" ".to_string());
        }
        let name = if first.is_ascii_alphabetic() {
            let start = self.position;
            while self.position < self.source.len()
                && self.source[self.position].is_ascii_alphabetic()
            {
                self.position += 1;
            }
            self.source[start..self.position].iter().collect::<String>()
        } else {
            self.position += 1;
            first.to_string()
        };

        if name == "\\" {
            return Some("\n".to_string());
        }
        if spacing_command(&name) {
            return Some(" ".to_string());
        }
        if negative_spacing_command(&name) {
            return Some(NEGATIVE_SPACE.to_string());
        }
        if ignored_command(&name) {
            return Some(String::new());
        }
        if matches!(
            name.as_str(),
            "bf" | "cal" | "it" | "rm" | "sf" | "sl" | "tt"
        ) {
            while self.position < self.source.len() && self.source[self.position].is_whitespace() {
                self.position += 1;
            }
            return Some(String::new());
        }
        if matches!(name.as_str(), "{" | "}" | "$" | "%" | "#" | "_" | "&") {
            return Some(name);
        }
        if name == "|" {
            return Some("‖".to_string());
        }
        if limit_operator(&name) {
            return self.parse_operator(name, true, true);
        }
        if let Some(glyph) = symbol(&name) {
            if matches!(
                name.as_str(),
                "int" | "sum" | "prod" | "oint" | "bigcap" | "bigcup"
            ) {
                return self.parse_operator(glyph.to_string(), true, false);
            }
            return Some(
                if name == "cdot" || name == "times" || relation_command(&name) {
                    format!(" {glyph} ")
                } else {
                    glyph.to_string()
                },
            );
        }
        if named_operator(&name) {
            return Some(format!("{NAME_START}{name}{NAME_END}"));
        }
        if matches!(name.as_str(), "left" | "middle" | "right") {
            if self.source.get(self.position) == Some(&'.') {
                self.position += 1;
            }
            return Some(String::new());
        }
        if matches!(name.as_str(), "frac" | "dfrac" | "tfrac") {
            let stack = self.display && self.stack_fractions && name != "tfrac";
            let numerator = self.parse_required_argument(!stack)?;
            let denominator = self.parse_required_argument(!stack)?;
            if stack {
                let index = self.nodes.len();
                self.nodes.push(LayoutNode::Fraction {
                    numerator: normalize_output(&numerator),
                    denominator: normalize_output(&denominator),
                });
                return Some(format!("{OPENER}{index}{CLOSER}"));
            }
            return Some(format_fraction(&numerator, &denominator));
        }
        if name == "sqrt" {
            let degree = self.parse_optional_argument()?;
            let value = self.parse_required_argument(true)?;
            return Some(match degree.as_str() {
                "" | "2" => format_root(&value, "√"),
                "3" => format_root(&value, "∛"),
                "4" => format_root(&value, "∜"),
                d => format!(
                    "{}{}",
                    format_script(d, ScriptKind::Sup),
                    format_root(&value, "√")
                ),
            });
        }
        if plain_wrapper(&name) {
            let value = self.parse_required_argument(true)?;
            return Some(if name.starts_with("text") || name == "mbox" {
                value
            } else {
                value.trim().to_string()
            });
        }
        if matches!(name.as_str(), "mod" | "bmod") {
            return Some(" mod ".to_string());
        }
        if matches!(name.as_str(), "overset" | "stackrel") {
            let upper = self.parse_required_argument(true)?;
            let value = self.parse_required_argument(true)?;
            return Some(format!(
                "{}{}",
                value.trim(),
                format_script(&upper, ScriptKind::Sup)
            ));
        }
        if name == "underset" {
            let lower = self.parse_required_argument(true)?;
            let value = self.parse_required_argument(true)?;
            return Some(format!(
                "{}{}",
                value.trim(),
                format_script(&lower, ScriptKind::Sub)
            ));
        }
        if matches!(
            name.as_str(),
            "acute" | "bar" | "dot" | "ddot" | "hat" | "tilde" | "vec" | "overline"
        ) {
            let accent = match name.as_str() {
                "acute" => "\u{0301}",
                "bar" | "overline" => "\u{0305}",
                "dot" => "\u{0307}",
                "ddot" => "\u{0308}",
                "hat" => "\u{0302}",
                "tilde" => "\u{0303}",
                "vec" => "\u{20d7}",
                _ => "",
            };
            let value = self.parse_required_argument(true)?;
            return Some(if value.chars().count() == 1 {
                format!("{value}{accent}")
            } else {
                format!("{name}({value})")
            });
        }
        // Unknown command: upstream marks the expression unsupported, which
        // makes the caller show the raw LaTeX source.
        None
    }

    fn parse_operator(
        &mut self,
        operator: String,
        spaced_limits: bool,
        spaced: bool,
    ) -> Option<String> {
        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut lookahead = self.position;
            while lookahead < self.source.len() && self.source[lookahead].is_whitespace() {
                lookahead += 1;
            }
            let kind = self.source.get(lookahead).copied();
            if kind != Some('_') && kind != Some('^') {
                break;
            }
            self.position = lookahead + 1;
            let value = normalize_output(&self.parse_required_argument(false)?).replace(' ', "");
            if kind == Some('_') {
                if lower.is_some() {
                    return None;
                }
                lower = Some(value);
            } else {
                if upper.is_some() {
                    return None;
                }
                upper = Some(value);
            }
        }
        if self.display && (lower.is_some() || upper.is_some()) {
            let index = self.nodes.len();
            self.nodes.push(LayoutNode::Operator {
                operator,
                lower,
                upper,
            });
            return Some(format!("{OPENER}{index}{CLOSER}"));
        }
        let mut rendered = operator;
        if let Some(lower) = lower {
            if spaced_limits {
                rendered.push('[');
                rendered.push_str(&lower);
                rendered.push(']');
            } else {
                rendered.push_str(&format_script(&lower, ScriptKind::Sub));
            }
        }
        if let Some(upper) = upper {
            rendered.push_str(&format_script(&upper, ScriptKind::Sup));
        }
        Some(if spaced {
            format!(" {rendered} ")
        } else {
            rendered
        })
    }

    fn parse_required_argument(&mut self, stack_fractions: bool) -> Option<String> {
        let previous = self.stack_fractions;
        self.stack_fractions = previous && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous;
        value
    }

    fn parse_required_argument_value(&mut self) -> Option<String> {
        while self.position < self.source.len() && self.source[self.position].is_whitespace() {
            self.position += 1;
        }
        let c = *self.source.get(self.position)?;
        if c == '{' {
            self.position += 1;
            return self.parse_sequence(Some('}'));
        }
        if c == '\\' {
            return self.parse_command();
        }
        self.position += 1;
        Some(c.to_string())
    }

    /// An absent optional argument is the empty string; `None` marks a
    /// malformed one (`latex.ts:1280-1295`).
    fn parse_optional_argument(&mut self) -> Option<String> {
        while self.position < self.source.len() && matches!(self.source[self.position], ' ' | '\t')
        {
            self.position += 1;
        }
        if self.source.get(self.position) != Some(&'[') {
            return Some(String::new());
        }
        let end = self.source[self.position + 1..]
            .iter()
            .position(|c| *c == ']')
            .map(|offset| offset + self.position + 1)?;
        let value: String = self.source[self.position + 1..end].iter().collect();
        self.position = end + 1;
        LatexParser::new(&value, false).render()
    }
}

/// Render a LaTeX expression as terminal-friendly Unicode text.
/// `None` means unsupported/malformed, exactly as upstream's `renderLatex`.
pub fn render_latex(source: &str, display: bool) -> Option<String> {
    LatexParser::new(source, display).render()
}

// ===========================================================================
// Lightweight syntax highlighter (`theme.ts:887-954`)
// ===========================================================================

/// The token kinds upstream's `CliHighlightTheme` maps onto theme colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxKind {
    Plain,
    Keyword,
    Type,
    Number,
    String,
    Comment,
    Function,
    Variable,
    Operator,
    Punctuation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Json,
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Shell,
}

fn language(lang: Option<&str>) -> Option<Language> {
    let name = lang?
        .split('+')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    Some(match name.as_str() {
        "json" | "jsonc" | "json5" => Language::Json,
        "rust" | "rs" => Language::Rust,
        "typescript" | "ts" => Language::TypeScript,
        "javascript" | "js" | "jsx" | "tsx" | "mjs" | "cjs" => Language::JavaScript,
        "python" | "py" => Language::Python,
        "bash" | "sh" | "shell" | "zsh" => Language::Shell,
        _ => return None,
    })
}

/// Whether this subset can highlight `lang` (`theme.ts:937`).
pub fn is_supported_language(lang: Option<&str>) -> bool {
    language(lang).is_some()
}

// Mirrors upstream's language keyword sets; split by language when extending.
#[allow(clippy::too_many_lines)]
fn keyword(lang: Language, word: &str) -> bool {
    let common = matches!(
        word,
        "if" | "else"
            | "for"
            | "while"
            | "return"
            | "break"
            | "continue"
            | "true"
            | "false"
            | "null"
            | "None"
            | "True"
            | "False"
            | "import"
            | "from"
            | "as"
    );
    common
        || match lang {
            Language::Json => matches!(word, "true" | "false" | "null"),
            Language::Rust => matches!(
                word,
                "fn" | "let"
                    | "mut"
                    | "const"
                    | "static"
                    | "struct"
                    | "enum"
                    | "trait"
                    | "impl"
                    | "pub"
                    | "use"
                    | "crate"
                    | "mod"
                    | "match"
                    | "loop"
                    | "in"
                    | "move"
                    | "ref"
                    | "type"
                    | "where"
                    | "async"
                    | "await"
                    | "dyn"
                    | "self"
                    | "Self"
                    | "super"
                    | "unsafe"
            ),
            Language::TypeScript | Language::JavaScript => matches!(
                word,
                "function"
                    | "var"
                    | "let"
                    | "const"
                    | "class"
                    | "extends"
                    | "new"
                    | "this"
                    | "typeof"
                    | "instanceof"
                    | "interface"
                    | "type"
                    | "export"
                    | "default"
                    | "async"
                    | "await"
                    | "yield"
                    | "try"
                    | "catch"
                    | "finally"
                    | "throw"
                    | "switch"
                    | "case"
                    | "delete"
                    | "void"
                    | "of"
                    | "in"
                    | "undefined"
            ),
            Language::Python => matches!(
                word,
                "def"
                    | "class"
                    | "lambda"
                    | "with"
                    | "try"
                    | "except"
                    | "finally"
                    | "raise"
                    | "pass"
                    | "yield"
                    | "assert"
                    | "del"
                    | "global"
                    | "nonlocal"
                    | "not"
                    | "and"
                    | "or"
                    | "is"
                    | "in"
                    | "async"
                    | "await"
                    | "print"
            ),
            Language::Shell => matches!(
                word,
                "echo"
                    | "cd"
                    | "export"
                    | "source"
                    | "local"
                    | "function"
                    | "if"
                    | "then"
                    | "fi"
                    | "for"
                    | "do"
                    | "done"
                    | "while"
                    | "case"
                    | "esac"
                    | "exit"
                    | "set"
                    | "unset"
                    | "read"
                    | "printf"
            ),
        }
}

fn builtin_type(lang: Language, word: &str) -> bool {
    matches!(
        word,
        "String"
            | "str"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "isize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "Vec"
            | "Option"
            | "Result"
            | "Box"
            | "Self"
    ) || (matches!(lang, Language::TypeScript | Language::JavaScript)
        && matches!(
            word,
            "number"
                | "string"
                | "boolean"
                | "any"
                | "unknown"
                | "never"
                | "void"
                | "object"
                | "symbol"
                | "bigint"
        ))
}

/// Accumulates adjacent same-kind runs into one segment.
#[derive(Default)]
struct Segments {
    lines: Vec<Vec<(SyntaxKind, String)>>,
    current_line: Vec<(SyntaxKind, String)>,
    pending: Option<(SyntaxKind, String)>,
}

impl Segments {
    fn emit(&mut self, kind: SyntaxKind, text: &str) {
        if let Some((previous_kind, previous_text)) = self.pending.as_mut() {
            if *previous_kind == kind {
                previous_text.push_str(text);
                return;
            }
            self.current_line
                .push((*previous_kind, std::mem::take(previous_text)));
        }
        self.pending = Some((kind, text.to_string()));
    }

    fn finish_line(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.current_line.push(pending);
        }
        self.lines.push(std::mem::take(&mut self.current_line));
    }
}

/// Tokenise one code block into per-line `(kind, text)` segments. The
/// concatenation of every segment equals the input, so layout is unchanged.
// One scanner for all languages, like upstream's single highlighter entry.
#[allow(clippy::too_many_lines)]
pub fn highlight_segments(code: &str, lang: Option<&str>) -> Vec<Vec<(SyntaxKind, String)>> {
    let Some(language) = language(lang) else {
        return code
            .split('\n')
            .map(|line| vec![(SyntaxKind::Plain, line.to_string())])
            .collect();
    };
    let mut out = Segments::default();
    let mut in_block_comment = false;
    for line in code.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        let mut index = 0usize;
        while index < chars.len() {
            let c = chars[index];
            if in_block_comment {
                let rest: String = chars[index..].iter().collect();
                if let Some(end) = rest.find("*/") {
                    let consumed = end + 2;
                    let text: String = chars[index..index + consumed].iter().collect();
                    out.emit(SyntaxKind::Comment, &text);
                    index += consumed;
                    in_block_comment = false;
                } else {
                    let text: String = chars[index..].iter().collect();
                    out.emit(SyntaxKind::Comment, &text);
                    index = chars.len();
                }
                continue;
            }
            let c_str = c.to_string();
            let line_comment = match language {
                Language::Python | Language::Shell => c == '#',
                Language::Json => false,
                _ => c == '/' && chars.get(index + 1) == Some(&'/'),
            };
            if line_comment {
                let text: String = chars[index..].iter().collect();
                out.emit(SyntaxKind::Comment, &text);
                break;
            }
            if matches!(
                language,
                Language::Rust | Language::TypeScript | Language::JavaScript | Language::Json
            ) && c == '/'
                && chars.get(index + 1) == Some(&'*')
            {
                let rest: String = chars[index + 2..].iter().collect();
                if let Some(end) = rest.find("*/") {
                    let consumed = end + 4;
                    let text: String = chars[index..index + consumed].iter().collect();
                    out.emit(SyntaxKind::Comment, &text);
                    index += consumed;
                } else {
                    let text: String = chars[index..].iter().collect();
                    out.emit(SyntaxKind::Comment, &text);
                    index = chars.len();
                    in_block_comment = true;
                }
                continue;
            }
            if c == '"' || c == '\'' || (matches!(language, Language::Python) && c == '`') {
                let quote = c;
                let mut end = index + 1;
                while end < chars.len() {
                    if chars[end] == '\\' {
                        end += 2;
                    } else if chars[end] == quote {
                        end += 1;
                        break;
                    } else {
                        end += 1;
                    }
                }
                let text: String = chars[index..end.min(chars.len())].iter().collect();
                let kind = if matches!(language, Language::Json)
                    && chars.get(end).is_some_and(|c| *c == ':')
                {
                    SyntaxKind::Variable
                } else {
                    SyntaxKind::String
                };
                out.emit(kind, &text);
                index = end;
                continue;
            }
            if (matches!(language, Language::Shell) && c == '$')
                || (matches!(language, Language::Python) && c == '@')
            {
                let mut end = index + 1;
                while end < chars.len()
                    && (chars[end].is_alphanumeric() || matches!(chars[end], '_' | '.'))
                {
                    end += 1;
                }
                if end > index + 1 {
                    let text: String = chars[index..end].iter().collect();
                    out.emit(SyntaxKind::Variable, &text);
                    index = end;
                    continue;
                }
            }
            if c.is_ascii_digit() {
                let mut end = index + 1;
                while end < chars.len()
                    && (chars[end].is_ascii_alphanumeric() || matches!(chars[end], '.' | '_'))
                {
                    end += 1;
                }
                let text: String = chars[index..end].iter().collect();
                out.emit(SyntaxKind::Number, &text);
                index = end;
                continue;
            }
            if c.is_alphabetic() || c == '_' {
                let mut end = index + 1;
                while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                    end += 1;
                }
                let word: String = chars[index..end].iter().collect();
                let kind = if keyword(language, &word) {
                    SyntaxKind::Keyword
                } else if builtin_type(language, &word) {
                    SyntaxKind::Type
                } else {
                    let mut lookahead = end;
                    while lookahead < chars.len() && chars[lookahead].is_whitespace() {
                        lookahead += 1;
                    }
                    if chars.get(lookahead) == Some(&'(') {
                        SyntaxKind::Function
                    } else {
                        SyntaxKind::Plain
                    }
                };
                out.emit(kind, &word);
                index = end;
                continue;
            }
            if "+-*/%=<>!&|~^?:".contains(c) {
                out.emit(SyntaxKind::Operator, &c_str);
            } else if "{}[]();,.".contains(c) {
                out.emit(SyntaxKind::Punctuation, &c_str);
            } else {
                out.emit(SyntaxKind::Plain, &c_str);
            }
            index += 1;
        }
        out.finish_line();
    }
    out.lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_latex_tokenizer_handles_dollar_and_bracket_forms() {
        let token = tokenize_inline_latex("$\\alpha + \\beta$ rest").expect("token");
        assert_eq!(token.text, "\\alpha + \\beta");
        assert_eq!(token.raw, "$\\alpha + \\beta$");
        assert!(!token.pending);
        let bracket = tokenize_inline_latex("\\(x^2\\) tail").expect("token");
        assert_eq!(bracket.text, "x^2");
        assert_eq!(bracket.raw, "\\(x^2\\)");
    }

    #[test]
    fn inline_latex_tokenizer_rejects_currency_and_accepts_pending_math() {
        assert!(tokenize_inline_latex("$5 and $6").is_none());
        assert!(tokenize_inline_latex("$ hello there").is_none());
        let pending = tokenize_inline_latex("$\\alpha").expect("pending token");
        assert!(pending.pending);
        assert_eq!(pending.text, "\\alpha");
    }

    #[test]
    fn block_latex_tokenizer_handles_display_math() {
        let token = tokenize_block_latex("$$\nx^2 + y^2\n$$\nnext").expect("token");
        assert_eq!(token.kind, LatexKind::Block);
        assert_eq!(token.text, "x^2 + y^2");
        assert_eq!(block_token_line_count(&token), 3);
    }

    #[test]
    fn render_latex_maps_symbols_scripts_and_fractions() {
        assert_eq!(
            render_latex("\\alpha + \\beta", false).as_deref(),
            Some("α + β")
        );
        assert_eq!(render_latex("x^2 + y_i", false).as_deref(), Some("x² + yᵢ"));
        assert_eq!(render_latex("\\frac{1}{2}", false).as_deref(), Some("1/2"));
        assert_eq!(render_latex("\\sqrt{x}", false).as_deref(), Some("√x"));
        assert_eq!(
            render_latex("\\sum_{i=1}^{n}i", false).as_deref(),
            Some("∑[i=1]ⁿi")
        );
    }

    #[test]
    fn render_latex_stacks_fractions_in_display_mode() {
        let rendered = render_latex("\\frac{a+b}{c}", true).expect("display fraction");
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains('─'), "fraction bar");
    }

    #[test]
    fn render_latex_rejects_unsupported_commands() {
        assert!(render_latex("\\begin{matrix}a\\end{matrix}", false).is_none());
    }

    #[test]
    fn highlighting_preserves_source_and_uses_theme_tokens() {
        let code = "{\"key\": 42}";
        let lines = highlight_segments(code, Some("json"));
        assert_eq!(lines.len(), 1);
        let joined: String = lines[0].iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(joined, code);
        assert!(
            lines[0]
                .iter()
                .any(|(kind, text)| *kind == SyntaxKind::Variable && text == "\"key\"")
        );
        assert!(
            lines[0]
                .iter()
                .any(|(kind, text)| *kind == SyntaxKind::Number && text == "42")
        );
    }

    #[test]
    fn highlighting_covers_rust_ts_shell_and_python() {
        for (lang, code) in [
            ("rust", "fn main() { let x = 1; }"),
            ("ts", "const x: number = 1;"),
            ("bash", "# comment\necho \"hi\""),
            ("python", "def f(x):\n    return x + 1"),
        ] {
            let lines = highlight_segments(code, Some(lang));
            let joined: String = lines
                .iter()
                .map(|segments| {
                    segments
                        .iter()
                        .map(|(_, text)| text.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<String>>()
                .join("\n");
            assert_eq!(joined, code);
            assert!(
                lines
                    .iter()
                    .flatten()
                    .any(|(kind, _)| *kind != SyntaxKind::Plain)
            );
        }
    }

    #[test]
    fn unsupported_languages_are_not_highlighted() {
        assert!(!is_supported_language(Some("brainfuck")));
        let lines = highlight_segments("hello", Some("brainfuck"));
        assert_eq!(lines, vec![vec![(SyntaxKind::Plain, "hello".to_string())]]);
    }
}
