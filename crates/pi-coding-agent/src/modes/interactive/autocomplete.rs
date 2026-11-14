//! The chat editor's autocomplete provider: slash commands, `@` fuzzy file
//! search via `fd`, and path-prefix completion via readdir.
//!
//! Ground truth: `packages/tui/src/autocomplete.ts` (`CombinedAutocompleteProvider`,
//! `autocomplete.ts:300-858`) plus the fuzzy engine in `packages/tui/src/fuzzy.ts`
//! (ported as [`pi_tui::fuzzy`]).
//!
//! Upstream's provider is async (fd subprocesses, 20 ms debounce, abort
//! tokens); the port is synchronous — the editor seam calls the provider
//! directly and an `fd` run blocks for its (typically few-ms) duration.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;

use pi_tui::components::editor::{AutocompleteProvider, Suggestions};
use pi_tui::components::select_list::SelectItem;
use pi_tui::fuzzy::fuzzy_filter;

/// `PATH_DELIMITERS` (`autocomplete.ts:8`).
fn is_path_delimiter(c: char) -> bool {
    matches!(c, ' ' | '\t' | '"' | '\'' | '=')
}

/// `autocompleteSeparatorRegex` (`utils.ts:62`): whitespace or CJK
/// punctuation. Upstream also splits on Han/Kana/Hangul script characters
/// that are `\p{Punctuation}`; the fullwidth punctuation class below covers
/// the practical cases without a unicode-script dependency.
fn is_autocomplete_separator(c: char) -> bool {
    c.is_whitespace() || CJK_PUNCTUATION.contains(c)
}

const CJK_PUNCTUATION: &str = "，．：；！？（）［］｛｝“”‘’…—";

/// Opening wrappers that may precede a path, mapped to their closing
/// counterpart (`autocomplete.ts:11`).
fn path_wrapper_closer(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '<' => Some('>'),
        '`' => Some('`'),
        _ => None,
    }
}

/// `toDisplayPath` (`autocomplete.ts:13`): backslashes become slashes.
fn to_display_path(value: &str) -> String {
    value.replace('\\', "/")
}

/// `escapeRegex` (`autocomplete.ts:17`).
fn escape_regex(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(
            c,
            '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `buildFdPathQuery` (`autocomplete.ts:21-44`): an fd glob/regex pattern for
/// the typed path, matching across `\` and `/` separators.
fn build_fd_path_query(query: &str) -> String {
    // Matches `\` or `/` between segments (`autocomplete.ts:33`).
    const SEPARATOR_PATTERN: &str = "[\\\\/]";

    let normalized = to_display_path(query);
    if !normalized.contains('/') {
        return escape_regex(&normalized);
    }

    let has_trailing_separator = normalized.ends_with('/');
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return normalized;
    }

    let segments: Vec<String> = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(escape_regex)
        .collect();

    let mut pattern = segments.join(SEPARATOR_PATTERN);
    if has_trailing_separator {
        pattern.push_str(SEPARATOR_PATTERN);
    }
    pattern
}

/// `findLastDelimiter` (`autocomplete.ts:46-56`): byte index of the last
/// delimiter, or `None`.
fn find_last_delimiter(text: &str) -> Option<usize> {
    text.char_indices()
        .filter(|(_, c)| is_path_delimiter(*c) || is_autocomplete_separator(*c))
        .map(|(i, _)| i)
        .next_back()
}

/// `stripLeadingWrappers` (`autocomplete.ts:60-70`): strip opening wrappers
/// before a path unless the token also contains its closer.
fn strip_leading_wrappers(token: &str) -> &str {
    let mut result = token;
    while !result.is_empty() {
        let Some(closer) = path_wrapper_closer(result.chars().next().expect("non-empty")) else {
            break;
        };
        if result[1..].contains(closer) {
            break;
        }
        result = &result[1..];
    }
    result
}

/// `findUnclosedQuoteStart` (`autocomplete.ts:72-86`): the byte index of the
/// opening `"` when inside quotes.
fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut quote_start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = Some(i);
            }
        }
    }
    quote_start.filter(|_| in_quotes)
}

/// `isTokenStart` (`autocomplete.ts:88-94`): walk back over opening wrappers,
/// then require a delimiter (or the start of the line) before the token.
fn is_token_start(text: &str, mut start: usize) -> bool {
    loop {
        if start == 0 {
            // `tokenStartRegex` matches the empty string via `^`.
            return true;
        }
        let previous = text[..start].chars().next_back().expect("boundary");
        if path_wrapper_closer(previous).is_some() {
            start -= previous.len_utf8();
            continue;
        }
        return is_path_delimiter(previous) || is_autocomplete_separator(previous);
    }
}

/// `extractQuotedPrefix` (`autocomplete.ts:96-114`): the byte index where a
/// quoted path prefix starts (`"src…` or `@"src…`), if the quote opens a
/// fresh token.
fn extract_quoted_prefix(text: &str) -> Option<usize> {
    let quote_start = find_unclosed_quote_start(text)?;

    if text[..quote_start].ends_with('@') {
        let at_index = quote_start - 1;
        if !is_token_start(text, at_index) {
            return None;
        }
        return Some(at_index);
    }

    if !is_token_start(text, quote_start) {
        return None;
    }
    Some(quote_start)
}

/// `parsePathPrefix` (`autocomplete.ts:116-127`).
fn parse_path_prefix(prefix: &str) -> (&str, bool, bool) {
    if let Some(raw) = prefix.strip_prefix("@\"") {
        return (raw, true, true);
    }
    if let Some(raw) = prefix.strip_prefix('"') {
        return (raw, false, true);
    }
    if let Some(raw) = prefix.strip_prefix('@') {
        return (raw, true, false);
    }
    (prefix, false, false)
}

/// `buildCompletionValue` (`autocomplete.ts:129-143`).
fn build_completion_value(path: &str, is_at_prefix: bool, is_quoted_prefix: bool) -> String {
    let needs_quotes = is_quoted_prefix || path.chars().any(is_autocomplete_separator);
    let prefix = if is_at_prefix { "@" } else { "" };
    if !needs_quotes {
        return format!("{prefix}{path}");
    }
    format!("{prefix}\"{path}\"")
}

/// `expandHomePath` (`autocomplete.ts:547-556`).
fn expand_home_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let Some(home) = std::env::var_os("HOME") else {
            return path.to_string();
        };
        let mut expanded = PathBuf::from(home);
        expanded.push(rest);
        let mut expanded = expanded.to_string_lossy().to_string();
        if path.ends_with('/') && !expanded.ends_with('/') {
            expanded.push('/');
        }
        expanded
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_else(|_| path.to_string())
    } else {
        path.to_string()
    }
}

/// Node's `path.dirname` (trailing slashes stripped first).
fn js_dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.starts_with('/') { "/" } else { "." }.to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => ".".to_string(),
    }
}

/// Node's `path.basename` (trailing slashes stripped first).
fn js_basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return path;
    }
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// `scoreEntry` (`autocomplete.ts:734-754`): higher = better match.
fn score_entry(file_path: &str, query: &str, is_directory: bool) -> f64 {
    let file_name = js_basename(file_path).to_lowercase();
    let lower_query = query.to_lowercase();

    let mut score = 0.0;

    if file_name == lower_query {
        score = 100.0;
    } else if file_name.starts_with(&lower_query) {
        score = 80.0;
    } else if file_name.contains(&lower_query) {
        score = 50.0;
    } else if file_path.to_lowercase().contains(&lower_query) {
        score = 30.0;
    }

    // Directories get a bonus to appear first.
    if is_directory && score > 0.0 {
        score += 10.0;
    }

    score
}

/// Slice the first `end` chars (editor cursor columns are char indices).
fn before_chars(text: &str, end: usize) -> String {
    text.chars().take(end).collect()
}

/// One `/`-command for the autocomplete list (`SlashCommand`,
/// `autocomplete.ts:254-261`).
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// `getArgumentCompletions`: argument suggestions for the text after the
    /// command's first space.
    pub argument_completions: Option<ArgumentCompletionsFn>,
}

/// `getArgumentCompletions`'s signature (`autocomplete.ts:260`), synchronous.
pub type ArgumentCompletionsFn =
    std::sync::Arc<dyn Fn(&str) -> Option<Vec<SelectItem>> + Send + Sync>;

impl SlashCommand {
    pub fn new(name: &str, description: &str) -> Self {
        SlashCommand {
            name: name.to_string(),
            description: Some(description.to_string()),
            argument_hint: None,
            argument_completions: None,
        }
    }

    #[must_use]
    pub fn with_argument_hint(mut self, hint: &str) -> Self {
        self.argument_hint = Some(hint.to_string());
        self
    }

    #[must_use]
    pub fn with_argument_completions(mut self, completions: ArgumentCompletionsFn) -> Self {
        self.argument_completions = Some(completions);
        self
    }
}

/// One `fd` output row.
struct FdEntry {
    path: String,
    is_directory: bool,
}

/// `extractAtPrefix` (`autocomplete.ts:500-514`).
fn extract_at_prefix(text: &str) -> Option<&str> {
    if let Some(start) = extract_quoted_prefix(text)
        && text[start..].starts_with("@\"")
    {
        return Some(&text[start..]);
    }

    let token = match find_last_delimiter(text) {
        Some(index) => strip_leading_wrappers(&text[index + 1..]),
        None => strip_leading_wrappers(text),
    };
    if token.starts_with('@') {
        return Some(token);
    }
    None
}

/// `extractPathPrefix` (`autocomplete.ts:517-544`).
fn extract_path_prefix(text: &str, force_extract: bool) -> Option<&str> {
    if let Some(start) = extract_quoted_prefix(text) {
        return Some(&text[start..]);
    }

    let path_prefix = match find_last_delimiter(text) {
        Some(index) => strip_leading_wrappers(&text[index + 1..]),
        None => strip_leading_wrappers(text),
    };

    // For forced extraction (Tab key), always return something.
    if force_extract {
        return Some(path_prefix);
    }

    // Natural triggers: only path-like prefixes.
    if path_prefix.contains('/') || path_prefix.starts_with('.') {
        return Some(path_prefix);
    }

    // An empty prefix after whitespace may be starting a path context.
    if path_prefix.is_empty() && !text.is_empty() && is_token_start(text, text.len()) {
        return Some(path_prefix);
    }

    None
}

/// `CombinedAutocompleteProvider` (`autocomplete.ts:300`).
pub struct CombinedAutocompleteProvider {
    commands: Vec<SlashCommand>,
    base_path: PathBuf,
    fd_path: Option<PathBuf>,
}

impl CombinedAutocompleteProvider {
    pub fn new(commands: Vec<SlashCommand>, base_path: PathBuf, fd_path: Option<PathBuf>) -> Self {
        CombinedAutocompleteProvider {
            commands,
            base_path,
            fd_path,
        }
    }

    /// `getFileSuggestions` (`autocomplete.ts:597-730`): readdir-based
    /// completion for a path prefix.
    #[allow(clippy::too_many_lines)]
    fn file_suggestions(&self, prefix: &str) -> Vec<SelectItem> {
        let (raw_prefix, is_at_prefix, is_quoted_prefix) = parse_path_prefix(prefix);
        let mut expanded_prefix = raw_prefix.to_string();
        if expanded_prefix.starts_with('~') {
            expanded_prefix = expand_home_path(&expanded_prefix);
        }

        let is_root_prefix = raw_prefix.is_empty()
            || raw_prefix == "./"
            || raw_prefix == "../"
            || raw_prefix == "~"
            || raw_prefix == "~/"
            || raw_prefix == "/";

        // `(dir_to_read, file_prefix)` for the readdir, plus whether the
        // prefix escapes the base path.
        let (search_dir, search_prefix) = if is_root_prefix || raw_prefix.ends_with('/') {
            if raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                (expanded_prefix.clone(), String::new())
            } else {
                (
                    self.base_path
                        .join(&expanded_prefix)
                        .to_string_lossy()
                        .to_string(),
                    String::new(),
                )
            }
        } else {
            let dir = js_dirname(&expanded_prefix);
            let file = js_basename(&expanded_prefix).to_string();
            if raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                (dir, file)
            } else {
                (
                    self.base_path.join(&dir).to_string_lossy().to_string(),
                    file,
                )
            }
        };

        let Ok(entries) = std::fs::read_dir(&search_dir) else {
            return Vec::new();
        };

        let lower_search_prefix = search_prefix.to_lowercase();
        let mut suggestions: Vec<(String, String, bool)> = Vec::new(); // (value, label, is_directory)

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.to_lowercase().starts_with(&lower_search_prefix) {
                continue;
            }

            // A directory, or a symlink pointing at one.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let mut is_directory = file_type.is_dir();
            if !is_directory && file_type.is_symlink() {
                is_directory = entry.path().is_dir();
            }

            let relative_path: String = if raw_prefix.ends_with('/') {
                format!("{raw_prefix}{name}")
            } else if raw_prefix.contains('/') || raw_prefix.contains('\\') {
                if let Some(home_relative_dir) = raw_prefix.strip_prefix("~/") {
                    let dir = js_dirname(home_relative_dir);
                    if dir == "." {
                        format!("~/{name}")
                    } else {
                        format!("~/{dir}/{name}")
                    }
                } else if raw_prefix.starts_with('/') {
                    let dir = js_dirname(raw_prefix);
                    if dir == "/" {
                        format!("/{name}")
                    } else {
                        format!("{dir}/{name}")
                    }
                } else {
                    let dir = js_dirname(raw_prefix);
                    let mut relative = if dir == "." {
                        name.clone()
                    } else {
                        format!("{dir}/{name}")
                    };
                    if raw_prefix.starts_with("./") && !relative.starts_with("./") {
                        relative = format!("./{relative}");
                    }
                    relative
                }
            } else if raw_prefix.starts_with('~') {
                format!("~/{name}")
            } else {
                name.clone()
            };

            let relative_path = to_display_path(&relative_path);
            let path_value = if is_directory {
                format!("{relative_path}/")
            } else {
                relative_path
            };
            let value = build_completion_value(&path_value, is_at_prefix, is_quoted_prefix);
            let label = if is_directory {
                format!("{name}/")
            } else {
                name
            };
            suggestions.push((value, label, is_directory));
        }

        // Sort directories first, then alphabetically (`autocomplete.ts:717-723`).
        // localeCompare approximation: case-insensitive, then byte order for
        // determinism.
        suggestions.sort_by(|a, b| match (a.2, b.2) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => {
                a.1.to_lowercase()
                    .cmp(&b.1.to_lowercase())
                    .then_with(|| a.1.cmp(&b.1))
            }
        });

        suggestions
            .into_iter()
            .map(|(value, label, _)| SelectItem {
                value,
                label,
                description: None,
            })
            .collect()
    }

    /// `walkDirectoryWithFd` (`autocomplete.ts:146-244`), synchronous.
    fn walk_directory_with_fd(
        &self,
        base_dir: &str,
        query: &str,
        max_results: usize,
        max_depth: Option<usize>,
    ) -> Vec<FdEntry> {
        let Some(fd_path) = &self.fd_path else {
            return Vec::new();
        };

        let mut args: Vec<String> = vec![
            "--base-directory".to_string(),
            base_dir.to_string(),
            "--max-results".to_string(),
            max_results.to_string(),
            "--type".to_string(),
            "f".to_string(),
            "--type".to_string(),
            "d".to_string(),
            "--follow".to_string(),
            "--hidden".to_string(),
            "--exclude".to_string(),
            ".git".to_string(),
            "--exclude".to_string(),
            ".git/*".to_string(),
            "--exclude".to_string(),
            ".git/**".to_string(),
        ];
        if let Some(depth) = max_depth {
            args.push("--max-depth".to_string());
            args.push(depth.to_string());
        }
        if to_display_path(query).contains('/') {
            args.push("--full-path".to_string());
        }
        if !query.is_empty() {
            args.push(build_fd_path_query(query));
        }

        let Ok(output) = std::process::Command::new(fd_path)
            .args(&args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return Vec::new();
        };
        if !output.status.success() || output.stdout.is_empty() {
            return Vec::new();
        }

        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                let display_line = to_display_path(line);
                let is_directory = display_line.ends_with('/');
                let normalized_path = if is_directory {
                    &display_line[..display_line.len() - 1]
                } else {
                    display_line.as_str()
                };
                if normalized_path == ".git"
                    || normalized_path.starts_with(".git/")
                    || normalized_path.contains("/.git/")
                {
                    return None;
                }
                Some(FdEntry {
                    path: display_line,
                    is_directory,
                })
            })
            .collect()
    }

    /// `resolveScopedFuzzyQuery` (`autocomplete.ts:558-586`).
    fn resolve_scoped_fuzzy_query(&self, raw_query: &str) -> Option<(String, String, String)> {
        let normalized_query = to_display_path(raw_query);
        let slash_index = normalized_query.rfind('/')?;
        let display_base = normalized_query[..=slash_index].to_string();
        let query = normalized_query[slash_index + 1..].to_string();

        let base_dir = if display_base.starts_with("~/") {
            expand_home_path(&display_base)
        } else if display_base.starts_with('/') {
            display_base.clone()
        } else {
            self.base_path
                .join(&display_base)
                .to_string_lossy()
                .to_string()
        };

        match std::fs::metadata(&base_dir) {
            Ok(metadata) if metadata.is_dir() => Some((base_dir, query, display_base)),
            _ => None,
        }
    }

    /// `getFuzzyFileSuggestions` (`autocomplete.ts:769-844`): `fd`-driven
    /// fuzzy search — the directory's own entries first, then a recursive
    /// walk, scored, de-duplicated, best 20.
    fn fuzzy_file_suggestions(&self, query: &str, is_quoted_prefix: bool) -> Vec<SelectItem> {
        if self.fd_path.is_none() {
            return Vec::new();
        }

        let scope = self.resolve_scoped_fuzzy_query(query);
        let (fd_base_dir, fd_query, display_base) = match &scope {
            Some((base_dir, query, display_base)) => {
                (base_dir.clone(), query.clone(), Some(display_base.clone()))
            }
            None => (
                self.base_path.to_string_lossy().to_string(),
                query.to_string(),
                None,
            ),
        };

        let base_dir_entries = self.walk_directory_with_fd(&fd_base_dir, &fd_query, 100, Some(1));
        let recursive_entries = self.walk_directory_with_fd(&fd_base_dir, &fd_query, 100, None);

        let mut entries = base_dir_entries;
        let mut seen: HashSet<String> = entries.iter().map(|entry| entry.path.clone()).collect();
        for entry in recursive_entries {
            if seen.insert(entry.path.clone()) {
                entries.push(entry);
            }
        }

        let mut ranked: Vec<(FdEntry, f64)> = entries
            .into_iter()
            .map(|entry| {
                let entry_score = if fd_query.is_empty() {
                    1.0
                } else {
                    score_entry(&entry.path, &fd_query, entry.is_directory)
                };
                (entry, entry_score)
            })
            .filter(|(_, score)| *score > 0.0)
            .collect();

        ranked.sort_by(|(a, a_score), (b, b_score)| {
            b_score
                .total_cmp(a_score)
                .then_with(|| {
                    let a_depth = to_display_path(&a.path)
                        .split('/')
                        .filter(|p| !p.is_empty())
                        .count();
                    let b_depth = to_display_path(&b.path)
                        .split('/')
                        .filter(|p| !p.is_empty())
                        .count();
                    a_depth.cmp(&b_depth)
                })
                .then_with(|| a.path.chars().count().cmp(&b.path.chars().count()))
                .then_with(|| a.path.cmp(&b.path))
        });

        ranked
            .into_iter()
            .take(20)
            .map(|(entry, _)| {
                let path_without_slash = if entry.is_directory {
                    entry.path[..entry.path.len() - 1].to_string()
                } else {
                    entry.path.clone()
                };
                let display_path = match &display_base {
                    Some(display_base) => {
                        scoped_path_for_display(display_base, &path_without_slash)
                    }
                    None => path_without_slash.clone(),
                };
                let entry_name = js_basename(&path_without_slash).to_string();
                let completion_path = if entry.is_directory {
                    format!("{display_path}/")
                } else {
                    display_path.clone()
                };
                let value = build_completion_value(&completion_path, true, is_quoted_prefix);
                SelectItem {
                    value,
                    label: if entry.is_directory {
                        format!("{entry_name}/")
                    } else {
                        entry_name
                    },
                    description: Some(display_path),
                }
            })
            .collect()
    }
}

/// `scopedPathForDisplay` (`autocomplete.ts:588-594`).
fn scoped_path_for_display(display_base: &str, relative_path: &str) -> String {
    let normalized = to_display_path(relative_path);
    if display_base == "/" {
        return format!("/{normalized}");
    }
    format!("{}{}", to_display_path(display_base), normalized)
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    /// `getSuggestions` (`autocomplete.ts:311-410`).
    fn suggest(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<Suggestions> {
        let current_line = lines.get(cursor_line)?;
        let text_before_cursor = before_chars(current_line, cursor_col);

        // @-prefix fuzzy file search first, even inside slash commands.
        if let Some(at_prefix) = extract_at_prefix(&text_before_cursor) {
            let (raw_prefix, _is_at, is_quoted_prefix) = parse_path_prefix(at_prefix);
            let suggestions = self.fuzzy_file_suggestions(raw_prefix, is_quoted_prefix);
            if suggestions.is_empty() {
                return None;
            }
            return Some(Suggestions {
                items: suggestions,
                prefix: at_prefix.to_string(),
            });
        }

        let command_text = text_before_cursor.trim_start();
        if !force && command_text.starts_with('/') {
            let space_index = command_text.find(' ');

            let Some(space_index) = space_index else {
                let prefix = &command_text[1..];
                let mut command_items: Vec<(String, String, Option<String>)> = Vec::new(); // (name, label, full_description)
                for command in &self.commands {
                    let hint = command.argument_hint.as_deref();
                    let desc = command.description.as_deref().unwrap_or("");
                    let full_desc = match (hint, desc) {
                        (Some(hint), "") => Some(hint.to_string()),
                        (Some(hint), desc) => Some(format!("{hint} — {desc}")),
                        (None, "") => None,
                        (None, desc) => Some(desc.to_string()),
                    };
                    command_items.push((command.name.clone(), command.name.clone(), full_desc));
                }

                // Bare-name matches first; `skill:` names also match on the
                // part after the prefix.
                let bare_name_matches: Vec<_> = fuzzy_filter(&command_items, prefix, |item| {
                    match item.0.strip_prefix("skill:") {
                        Some(stripped) => stripped.to_string(),
                        None => item.0.clone(),
                    }
                });
                let bare_name_set: HashSet<&str> = bare_name_matches
                    .iter()
                    .map(|item| item.0.as_str())
                    .collect();
                let full_name_only_matches: Vec<_> = fuzzy_filter(
                    &command_items
                        .iter()
                        .filter(|item| {
                            item.0.starts_with("skill:") && !bare_name_set.contains(item.0.as_str())
                        })
                        .cloned()
                        .collect::<Vec<_>>(),
                    prefix,
                    |item| item.0.clone(),
                );

                let filtered: Vec<SelectItem> = bare_name_matches
                    .into_iter()
                    .chain(full_name_only_matches)
                    .map(|(name, label, description)| SelectItem {
                        value: name,
                        label,
                        description,
                    })
                    .collect();

                if filtered.is_empty() {
                    return None;
                }

                return Some(Suggestions {
                    items: filtered,
                    prefix: command_text.to_string(),
                });
            };

            let command_name = &command_text[1..space_index];
            let argument_text = &command_text[space_index + 1..];

            let command = self
                .commands
                .iter()
                .find(|command| command.name == command_name)?;
            let completions = command.argument_completions.as_ref()?;
            let argument_suggestions = completions(argument_text)?;
            if argument_suggestions.is_empty() {
                return None;
            }

            return Some(Suggestions {
                items: argument_suggestions,
                prefix: argument_text.to_string(),
            });
        }

        let path_match = extract_path_prefix(&text_before_cursor, force)?;
        let suggestions = self.file_suggestions(path_match);
        if suggestions.is_empty() {
            return None;
        }

        Some(Suggestions {
            items: suggestions,
            prefix: path_match.to_string(),
        })
    }

    /// `applyCompletion` (`autocomplete.ts:412-497`).
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &SelectItem,
        prefix: &str,
    ) -> (Vec<String>, usize, usize) {
        let current_line = lines.get(cursor_line).cloned().unwrap_or_default();
        let prefix_len = prefix.chars().count();
        let before_prefix = before_chars(&current_line, cursor_col.saturating_sub(prefix_len));
        let after_cursor: String = current_line.chars().skip(cursor_col).collect();

        let is_quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
        let has_leading_quote_after_cursor = after_cursor.starts_with('"');
        let has_trailing_quote_in_item = item.value.ends_with('"');
        let adjusted_after_cursor =
            if is_quoted_prefix && has_trailing_quote_in_item && has_leading_quote_after_cursor {
                after_cursor[1..].to_string()
            } else {
                after_cursor
            };

        let before_len = before_prefix.chars().count();

        // Slash command at the start of the line.
        let is_slash_command = prefix.starts_with('/')
            && before_prefix.trim().is_empty()
            && !prefix[1..].contains('/');
        if is_slash_command {
            let new_line = format!("{before_prefix}/{} {adjusted_after_cursor}", item.value);
            let mut lines = lines.to_vec();
            lines[cursor_line] = new_line;
            return (
                lines,
                cursor_line,
                before_len + item.value.chars().count() + 2,
            );
        }

        // File attachment: no space after directories so completion continues.
        if prefix.starts_with('@') {
            let is_directory = item.label.ends_with('/');
            let suffix = if is_directory { "" } else { " " };
            let new_line = format!(
                "{before_prefix}{}{suffix}{adjusted_after_cursor}",
                item.value
            );
            let mut lines = lines.to_vec();
            lines[cursor_line] = new_line;
            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.chars().count() - 1
            } else {
                item.value.chars().count()
            };
            return (
                lines,
                cursor_line,
                before_len + cursor_offset + suffix.chars().count(),
            );
        }

        // Upstream splits the tail into a command-argument branch and a plain
        // path branch, but both produce identical text and cursor math, so
        // they share one implementation here.
        let is_directory = item.label.ends_with('/');
        let has_trailing_quote = item.value.ends_with('"');
        let cursor_offset = if is_directory && has_trailing_quote {
            item.value.chars().count() - 1
        } else {
            item.value.chars().count()
        };

        let mut lines = lines.to_vec();
        lines[cursor_line] = format!("{before_prefix}{}{adjusted_after_cursor}", item.value);
        (lines, cursor_line, before_len + cursor_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    use crate::tools::bash::which;

    fn provider(base: &Path, fd: bool) -> CombinedAutocompleteProvider {
        CombinedAutocompleteProvider::new(
            vec![
                SlashCommand::new("model", "Select model (opens selector UI)")
                    .with_argument_hint("<provider/model>"),
                SlashCommand::new("thinking", "Set thinking level").with_argument_hint("<level>"),
                SlashCommand::new("new", "Start a new session"),
                SlashCommand::new("quit", "Quit pi"),
            ],
            base.to_path_buf(),
            if fd { which("fd") } else { None },
        )
    }

    fn suggest_at(
        provider: &CombinedAutocompleteProvider,
        line: &str,
        col: usize,
        force: bool,
    ) -> Suggestions {
        provider
            .suggest(&[line.to_string()], 0, col, force)
            .expect("suggestions")
    }

    #[test]
    fn slash_commands_filter_by_fuzzy_prefix() {
        let provider = provider(Path::new("/tmp"), false);
        let suggestions = suggest_at(&provider, "/mo", 3, false);
        assert_eq!(suggestions.prefix, "/mo");
        assert!(suggestions.items.iter().any(|item| item.value == "model"));
        // Descriptions compose the argument hint (`autocomplete.ts:342-345`).
        let model = suggestions
            .items
            .iter()
            .find(|item| item.value == "model")
            .expect("model");
        assert_eq!(
            model.description.as_deref(),
            Some("<provider/model> — Select model (opens selector UI)")
        );

        // No match → no suggestions.
        assert!(
            provider
                .suggest(&["/zzz".to_string()], 0, 4, false)
                .is_none()
        );
    }

    #[test]
    fn forced_tab_skips_slash_completion() {
        // An empty base: forced path extraction finds nothing, so the whole
        // suggest is None — proving the slash branch was skipped.
        let base = std::env::temp_dir().join(format!("pi-autoc-empty-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("dir");
        let provider = provider(&base, false);
        assert!(provider.suggest(&["/mo".to_string()], 0, 3, true).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn path_completion_lists_directories_first() {
        let base = std::env::temp_dir().join(format!("pi-autoc-{}", std::process::id()));
        fs::create_dir_all(base.join("src/deep")).expect("dirs");
        fs::write(base.join("src/main.rs"), "fn main() {}").expect("file");
        fs::write(base.join("README.md"), "# t").expect("file");

        let provider = provider(&base, false);

        // "sr" is not path-like, so natural typing does not complete it —
        // only an explicit Tab (force) does (`autocomplete.ts:527-536`).
        assert!(provider.suggest(&["sr".to_string()], 0, 2, false).is_none());
        let suggestions = suggest_at(&provider, "sr", 2, true);
        assert_eq!(suggestions.prefix, "sr");
        assert_eq!(suggestions.items.len(), 1);
        assert_eq!(suggestions.items[0].value, "src/");
        assert_eq!(suggestions.items[0].label, "src/");

        // "src/" is path-like, so it completes naturally.
        let suggestions = suggest_at(&provider, "src/", 4, false);
        let values: Vec<&str> = suggestions.items.iter().map(|i| i.value.as_str()).collect();
        assert_eq!(values, vec!["src/deep/", "src/main.rs"]);

        // Standalone names complete with an explicit Tab.
        let suggestions = suggest_at(&provider, "READ", 4, true);
        assert_eq!(suggestions.items[0].value, "README.md");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn quoted_prefixes_complete_and_requote() {
        let base = std::env::temp_dir().join(format!("pi-autoc-q-{}", std::process::id()));
        fs::create_dir_all(base.join("src")).expect("dir");
        fs::write(base.join("src/with space.txt"), "x").expect("file");

        let provider = provider(&base, false);

        // `"src/with` — the quoted prefix produces a quoted value.
        let suggestions = suggest_at(&provider, "\"src/with", 9, false);
        assert_eq!(suggestions.prefix, "\"src/with");
        assert_eq!(suggestions.items[0].value, "\"src/with space.txt\"");

        // Applying it merges the closing quote with the one after the cursor.
        let (lines, _line, col) = provider.apply_completion(
            &["\"src/with\"".to_string()],
            0,
            9,
            &suggestions.items[0],
            &suggestions.prefix,
        );
        assert_eq!(lines[0], "\"src/with space.txt\"");
        assert_eq!(col, lines[0].chars().count());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn at_prefix_apply_appends_space_for_files_only() {
        let provider = provider(Path::new("/tmp"), false);
        let item = SelectItem::new("@src/main.rs", "main.rs");

        let (lines, _line, col) =
            provider.apply_completion(&["@src/ma".to_string()], 0, 7, &item, "@src/ma");
        assert_eq!(lines[0], "@src/main.rs ");
        assert_eq!(col, "@src/main.rs ".chars().count());

        let dir_item = SelectItem::new("@src/", "src/");
        let (lines, _line, col) =
            provider.apply_completion(&["@src/".to_string()], 0, 5, &dir_item, "@src/");
        assert_eq!(lines[0], "@src/");
        assert_eq!(col, 5);
    }

    #[test]
    fn command_argument_completions_replace_only_the_argument() {
        let thinking = SlashCommand::new("thinking", "Set thinking level")
            .with_argument_hint("<level>")
            .with_argument_completions(std::sync::Arc::new(|prefix: &str| {
                let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
                let filtered: Vec<SelectItem> = levels
                    .iter()
                    .filter(|level| level.starts_with(prefix))
                    .map(|level| SelectItem::new(*level, *level))
                    .collect();
                if filtered.is_empty() {
                    None
                } else {
                    Some(filtered)
                }
            }));
        let provider =
            CombinedAutocompleteProvider::new(vec![thinking], PathBuf::from("/tmp"), None);

        let suggestions = suggest_at(&provider, "/thinking hi", 12, false);
        assert_eq!(suggestions.prefix, "hi");
        assert_eq!(suggestions.items[0].value, "high");

        // Applying keeps the "/thinking " part and adds no trailing space.
        let (lines, _line, col) = provider.apply_completion(
            &["/thinking hi".to_string()],
            0,
            12,
            &suggestions.items[0],
            &suggestions.prefix,
        );
        assert_eq!(lines[0], "/thinking high");
        assert_eq!(col, "/thinking high".chars().count());
    }

    #[test]
    fn fuzzy_at_prefix_uses_fd_when_available() {
        let Some(fd) = which("fd") else {
            eprintln!("skipping: fd not installed");
            return;
        };
        let base = std::env::temp_dir().join(format!("pi-autoc-fd-{}", std::process::id()));
        fs::create_dir_all(base.join("src")).expect("dir");
        fs::write(base.join("src/main.rs"), "fn main() {}").expect("file");

        let provider = CombinedAutocompleteProvider::new(Vec::new(), base.clone(), Some(fd));
        let suggestions = suggest_at(&provider, "@main", 5, false);
        assert_eq!(suggestions.prefix, "@main");
        assert!(
            suggestions
                .items
                .iter()
                .any(|item| item.value == "@src/main.rs"),
            "expected @src/main.rs, got {:?}",
            suggestions.items
        );
        // The description shows the path relative to the base directory.
        assert!(
            suggestions
                .items
                .iter()
                .any(|item| item.description.as_deref() == Some("src/main.rs"))
        );

        // Scoped query: "@src/mai" completes inside src/.
        let suggestions = suggest_at(&provider, "@src/mai", 8, false);
        assert!(
            suggestions
                .items
                .iter()
                .any(|item| item.value == "@src/main.rs")
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_and_delimiter_helpers_match_upstream() {
        // Path prefix parsing.
        assert_eq!(parse_path_prefix("@\"src"), ("src", true, true));
        assert_eq!(parse_path_prefix("\"src"), ("src", false, true));
        assert_eq!(parse_path_prefix("@src"), ("src", true, false));
        assert_eq!(parse_path_prefix("src"), ("src", false, false));

        // Quoted-prefix extraction: an unclosed quote after a delimiter IS a
        // quoted token (`isTokenStart` passes on the space); mid-word it isn't.
        assert_eq!(extract_quoted_prefix("say \"hel"), Some(4));
        assert_eq!(extract_quoted_prefix("say\"hel"), None);
        assert_eq!(extract_quoted_prefix("look @\"sr"), Some(5)); // starts at the @
        assert_eq!(extract_quoted_prefix("\"sr"), Some(0));

        // Wrappers are stripped unless the closer is present.
        assert_eq!(strip_leading_wrappers("(~/Dev"), "~/Dev");
        assert_eq!(strip_leading_wrappers("`src/ma"), "src/ma");
        assert_eq!(strip_leading_wrappers("app/[slug]/pa"), "app/[slug]/pa");

        // Delimiters end the token.
        assert_eq!(find_last_delimiter("run src=foo bar"), Some(11));
        assert_eq!(find_last_delimiter("plain"), None);

        assert_eq!(build_completion_value("a b/c", true, false), "@\"a b/c\"");
        assert_eq!(build_completion_value("a/c", false, false), "a/c");
    }
}
