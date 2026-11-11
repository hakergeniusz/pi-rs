//! Skill discovery and packaging.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/skills.ts` plus
//! `utils/frontmatter.ts`.
//!
//! Discovery rules (`skills.ts:160-167`):
//!
//! * a directory that contains `SKILL.md` is a skill root and is not recursed
//!   into further — the root `SKILL.md` wins over nested ones;
//! * otherwise, direct `.md` children in the root load as skills (their name
//!   comes from their parent directory) and subdirectories recurse;
//! * `.gitignore` / `.ignore` / `.fdignore` files are honored, with patterns
//!   prefixed by their directory relative to the scan root
//!   (`skills.ts:24-65`); hidden entries and `node_modules` are skipped.
//!
//! The frontmatter reader is [`parse_frontmatter`], the port of
//! `utils/frontmatter.ts`, backed by a YAML subset parser: the flat
//! `key: value` mappings skill and prompt-template frontmatter use (plain,
//! quoted, and block scalars), with the `yaml` package's error positions for
//! the malformed cases pi reports.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::tools::CONFIG_DIR_NAME;

/// Max name length per spec (`skills.ts:11`).
const MAX_NAME_LENGTH: usize = 64;
/// Max description length per spec (`skills.ts:14`).
const MAX_DESCRIPTION_LENGTH: usize = 1024;
/// pi's `IGNORE_FILE_NAMES` (`skills.ts:16`).
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

// ---------------------------------------------------------------------------
// Frontmatter (utils/frontmatter.ts)
// ---------------------------------------------------------------------------

/// A parsed frontmatter value, the subset of YAML the tools read.
#[derive(Debug, Clone, PartialEq)]
pub enum FrontmatterValue {
    Text(String),
    Boolean(bool),
    Null,
    /// Numbers, arrays, and anything else — present but not string-shaped.
    Other,
}

impl FrontmatterValue {
    /// The scalar as text, or `None` for anything that is not a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FrontmatterValue::Text(text) => Some(text),
            _ => None,
        }
    }
}

/// pi's `ParsedFrontmatter` (`utils/frontmatter.ts:4-7`).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFrontmatter {
    /// Key order follows the file, which is what the TS object keeps.
    pub frontmatter: Vec<(String, FrontmatterValue)>,
    pub body: String,
}

impl ParsedFrontmatter {
    /// `frontmatter[key]` for a string-valued key.
    pub fn get(&self, key: &str) -> Option<&FrontmatterValue> {
        self.frontmatter
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }
}

/// pi's `extractFrontmatter` (`utils/frontmatter.ts:15-33`).
fn extract_frontmatter(content: &str) -> (Option<String>, String) {
    let normalized = normalize_newlines(&strip_bom(content));
    if !normalized.starts_with("---") {
        return (None, normalized);
    }
    let Some(end_index) = normalized[3.min(normalized.len())..]
        .find("\n---")
        .map(|relative| 3.min(normalized.len()) + relative)
    else {
        return (None, normalized);
    };
    let yaml_string = normalized[4.min(end_index)..end_index].to_string();
    let body = normalized[end_index + 4..].trim().to_string();
    (Some(yaml_string), body)
}

/// pi's `normalizeNewlines`.
fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

/// pi's `stripBom` (`utils/text.ts`).
fn strip_bom(content: &str) -> String {
    content
        .strip_prefix('\u{feff}')
        .unwrap_or(content)
        .to_string()
}

/// pi's `parseFrontmatter` (`utils/frontmatter.ts:40-52`).
///
/// The YAML half is a subset parser for flat mappings; the error messages it
/// returns carry the `yaml` package's `at line L, column C` positions so the
/// diagnostics pi surfaces read the same.
pub fn parse_frontmatter(content: &str) -> Result<ParsedFrontmatter, String> {
    let (yaml_string, body) = extract_frontmatter(content);
    let Some(yaml_string) = yaml_string else {
        return Ok(ParsedFrontmatter {
            frontmatter: Vec::new(),
            body,
        });
    };
    let frontmatter = parse_yaml_mapping(&yaml_string)?;
    Ok(ParsedFrontmatter { frontmatter, body })
}

/// pi's `stripFrontmatter`.
pub fn strip_frontmatter(content: &str) -> String {
    parse_frontmatter(content)
        .map(|parsed| parsed.body)
        .unwrap_or_default()
}

/// Parse the flat `key: value` mapping the frontmatter is.
fn parse_yaml_mapping(yaml: &str) -> Result<Vec<(String, FrontmatterValue)>, String> {
    let mut entries: Vec<(String, FrontmatterValue)> = Vec::new();
    let mut lines = yaml.lines().enumerate().peekable();
    while let Some((index, raw_line)) = lines.next() {
        let line_number = index + 1;
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        let Some(colon) = line.find(':') else {
            return Err(format!(
                "A mapping entry needs a `key: value` form at line {line_number}, column {}",
                indent + 1
            ));
        };
        let key = line[..colon].trim().to_string();
        if key.is_empty() {
            return Err(format!(
                "A mapping entry needs a key before the colon at line {line_number}, column {}",
                indent + 1
            ));
        }
        let after_colon = &line[colon + 1..];
        let value_offset = colon + 1 + (after_colon.len() - after_colon.trim_start().len());
        let value_text = after_colon.trim_start();
        if value_text.is_empty() {
            // `key:` with nothing: either a null scalar or the header of a
            // nested mapping, which the frontmatter subset does not support.
            match lines.peek() {
                Some((_, next_line)) if next_line.starts_with(&" ".repeat(indent + 1)) => {
                    return Err(format!(
                        "Nested mappings are not supported in frontmatter at line {}, column {}",
                        index + 2,
                        indent + 2
                    ));
                }
                _ => entries.push((key, FrontmatterValue::Null)),
            }
            continue;
        }

        // Block scalars: `|` / `>` with optional chomping indicators.
        if matches!(value_text, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            let (text, next_index) = parse_block_scalar(yaml, index, indent, value_text);
            entries.push((key, FrontmatterValue::Text(text)));
            for _ in 0..next_index - index - 1 {
                lines.next();
            }
            continue;
        }

        // Plain and quoted scalars; a nested `: ` inside a plain scalar is
        // the `yaml` package's compact-mapping error (`description: a: b`).
        let (value, error) =
            parse_scalar(value_text, line_number, value_offset + 1, line.len() + 1);
        if let Some(error) = error {
            return Err(error);
        }
        entries.push((key, value));
    }
    Ok(entries)
}

/// Parse one scalar. `value_start` is its 1-based column, `past_end` the
/// column just past the line, both for error messages. The error the `yaml`
/// package would raise travels as the optional string, so no `Result` is
/// needed.
#[allow(clippy::type_complexity)]
fn parse_scalar(
    value_text: &str,
    line_number: usize,
    value_start: usize,
    past_end: usize,
) -> (FrontmatterValue, Option<String>) {
    let first = value_text.as_bytes().first().copied().unwrap_or(b' ');
    match first {
        b'"' | b'\'' => {
            let quote = first as char;
            match find_closing_quote(value_text, quote) {
                Some(end) => {
                    let inner = &value_text[1..end];
                    let value = if quote == '"' {
                        unescape_double_quoted(inner)
                    } else {
                        inner.to_string()
                    };
                    (FrontmatterValue::Text(value), None)
                }
                None => (
                    FrontmatterValue::Null,
                    Some(format!(
                        "Unterminated quoted scalar at line {line_number}, column {value_start}"
                    )),
                ),
            }
        }
        b'[' => {
            if value_text.contains(']') {
                (FrontmatterValue::Other, None)
            } else {
                (
                    FrontmatterValue::Null,
                    Some(format!(
                        "Flow sequence in block collection must be sufficiently indented and end with a ] at line {line_number}, column {past_end}"
                    )),
                )
            }
        }
        b'{' => {
            if value_text.contains('}') {
                (FrontmatterValue::Other, None)
            } else {
                (
                    FrontmatterValue::Null,
                    Some(format!(
                        "Flow mapping in block collection must be sufficiently indented and end with a }} at line {line_number}, column {past_end}"
                    )),
                )
            }
        }
        _ => {
            // Plain scalar: a trailing ` # comment` is stripped, and a second
            // `: ` is a compact nested mapping.
            let without_comment = strip_inline_comment(value_text);
            let trimmed = without_comment.trim_end();
            if find_plain_colon_space(trimmed).is_some() {
                return (
                    FrontmatterValue::Null,
                    Some(format!(
                        "Nested mappings are not allowed in compact mappings at line {line_number}, column {value_start}"
                    )),
                );
            }
            (resolve_plain_scalar(trimmed), None)
        }
    }
}

/// `#` starts a comment when preceded by whitespace (`utils/frontmatter.ts`
/// relies on the `yaml` package for this).
fn strip_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    for index in 1..bytes.len() {
        if bytes[index] == b'#' && (bytes[index - 1] == b' ' || bytes[index - 1] == b'\t') {
            return &value[..index];
        }
    }
    value
}

/// The `: ` sequence inside a plain scalar that makes it a compact mapping.
fn find_plain_colon_space(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    (1..bytes.len().saturating_sub(1)).find(|&index| {
        bytes[index] == b':' && (bytes[index + 1] == b' ' || bytes[index + 1] == b'\t')
    })
}

/// The byte index of the closing quote, honoring backslash escapes in double
/// quotes and doubled quotes in single quotes.
fn find_closing_quote(value: &str, quote: char) -> Option<usize> {
    let mut chars = value.char_indices().peekable();
    chars.next(); // Skip the opening quote.
    while let Some((index, character)) = chars.next() {
        if quote == '"' && character == '\\' {
            chars.next(); // Skip the escaped character.
            continue;
        }
        if character == quote {
            if quote == '\'' && chars.peek() == Some(&(index + 1, '\'')) {
                chars.next();
                continue;
            }
            return Some(index);
        }
    }
    None
}

/// YAML double-quote escapes, the ones the frontmatter carries.
fn unescape_double_quoted(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(escaped @ ('"' | '\\')) => out.push(escaped),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// YAML plain-scalar resolution for the booleans and nulls frontmatter uses;
/// everything else stays text (`disable-model-invocation: true` is the case
/// that matters).
fn resolve_plain_scalar(value: &str) -> FrontmatterValue {
    match value {
        "true" | "True" | "TRUE" => FrontmatterValue::Boolean(true),
        "false" | "False" | "FALSE" => FrontmatterValue::Boolean(false),
        "null" | "Null" | "NULL" | "~" => FrontmatterValue::Null,
        other => FrontmatterValue::Text(other.to_string()),
    }
}

/// Parse a `|` / `>` block scalar body starting after `header_index`.
/// Returns the text and the index of the next non-block line. A block body
/// always parses (the loop stops at the first less-indented line), so this
/// cannot fail.
fn parse_block_scalar(
    yaml: &str,
    header_index: usize,
    key_indent: usize,
    header: &str,
) -> (String, usize) {
    let all_lines: Vec<&str> = yaml.split('\n').collect();
    let folded = header.starts_with('>');
    let chomp_strip = header.ends_with('-');
    let chomp_keep = header.ends_with('+');

    let mut body_lines: Vec<String> = Vec::new();
    let mut block_indent: Option<usize> = None;
    let mut index = header_index + 1;
    while index < all_lines.len() {
        let line = all_lines[index];
        if line.trim().is_empty() {
            body_lines.push(String::new());
            index += 1;
            continue;
        }
        let line_indent = line.len() - line.trim_start_matches(' ').len();
        if line_indent < key_indent + 1 {
            break;
        }
        // The first content line sets the block's indentation, which every
        // later line strips.
        let indent = *block_indent.get_or_insert(line_indent);
        let content = if line.len() > indent {
            line[indent..].to_string()
        } else {
            String::new()
        };
        body_lines.push(content);
        index += 1;
    }

    // Trim trailing empty lines for chomping, remembering how many there were.
    let mut trailing = 0;
    while body_lines.last().is_some_and(std::string::String::is_empty) {
        body_lines.pop();
        trailing += 1;
    }

    let mut text = String::new();
    if folded {
        let mut first = true;
        for line in &body_lines {
            if first {
                first = false;
            } else if line.is_empty() {
                text.push('\n');
            } else if text.ends_with('\n') {
                text.push_str(line);
                continue;
            } else {
                text.push(' ');
            }
            text.push_str(line);
        }
    } else {
        text.push_str(&body_lines.join("\n"));
    }
    if !chomp_strip && !body_lines.is_empty() {
        text.push('\n');
        if chomp_keep {
            for _ in 0..trailing {
                text.push('\n');
            }
        }
    }
    // `index` is one past the last consumed line, i.e. the first line past
    // the block, which is what the caller's skip count is based on.
    (text, index)
}

// ---------------------------------------------------------------------------
// Skills (skills.ts)
// ---------------------------------------------------------------------------

/// pi's `SkillFrontmatter` fields the tools read.
#[derive(Debug, Clone, Default)]
pub struct SkillFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub disable_model_invocation: bool,
}

/// pi's `Skill` (`skills.ts:74-81`).
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    /// pi's `sourceInfo.source`, the identifier the caller gave this scan.
    pub source: String,
    /// pi's `sourceInfo.scope`.
    pub scope: Scope,
    pub disable_model_invocation: bool,
}

/// pi's `SourceScope`, reduced to the scopes skill loading produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
    Temporary,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
            Scope::Temporary => "temporary",
        }
    }
}

/// pi's `ResourceDiagnostic` (`core/diagnostics.ts`), reduced to what skill
/// and prompt-template loading emit.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceDiagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
    pub path: PathBuf,
    /// pi's `collision` payload (`core/diagnostics.ts`), set on collisions.
    pub collision: Option<serde_json::Value>,
}

/// pi's diagnostic `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    Warning,
    Collision,
}

/// pi's `LoadSkillsResult` (`skills.ts:83-86`).
#[derive(Debug, Clone, Default)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// pi's `{ skill, diagnostics }` pair `loadSkillFromFile` returns
/// (`skills.ts:277-345`).
#[derive(Debug, Clone, Default)]
pub struct LoadedSkill {
    pub skill: Option<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// pi's `LoadSkillsFromDirOptions` (`skills.ts:129-134`).
pub struct LoadSkillsFromDirOptions {
    /// Directory to scan for skills.
    pub dir: PathBuf,
    /// Source identifier for these skills.
    pub source: String,
}

/// pi's `validateName` (`skills.ts:92-112`).
fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let char_count = name.chars().count();
    if char_count > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({char_count})"
        ));
    }
    if !name.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    }) {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

/// pi's `validateDescription` (`skills.ts:117-127`).
fn validate_description(description: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    match description {
        None => errors.push("description is required".to_string()),
        Some(description) if description.trim().is_empty() => {
            errors.push("description is required".to_string());
        }
        Some(description) => {
            let char_count = description.chars().count();
            if char_count > MAX_DESCRIPTION_LENGTH {
                errors.push(format!(
                    "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({char_count})"
                ));
            }
        }
    }
    errors
}

/// pi's `prefixIgnorePattern` (`skills.ts:24-45`): anchor an ignore rule at
/// the directory it was found in.
fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }

    let mut pattern = line.trim_end();
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest;
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest;
    }
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    let prefixed = if prefix.is_empty() {
        pattern.to_string()
    } else {
        format!("{prefix}{pattern}")
    };
    Some(if negated {
        format!("!{prefixed}")
    } else {
        prefixed
    })
}

/// One ignore file's rules, the subset of gitignore semantics the scan needs:
/// `gitignore`-style glob patterns with `!` negation, matched against paths
/// relative to the scan root.
#[derive(Debug, Clone, Default)]
struct IgnoreMatcher {
    /// (pattern with `!` stripped, negated)
    rules: Vec<(globset::Glob, bool)>,
}

impl IgnoreMatcher {
    fn add_rules(&mut self, patterns: &[String]) {
        for pattern in patterns {
            let (pattern, negated) = match pattern.strip_prefix('!') {
                Some(rest) => (rest.to_string(), true),
                None => (pattern.clone(), false),
            };
            if let Ok(glob) = globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
            {
                self.rules.push((glob, negated));
            }
        }
    }

    /// `ignore().ignores(relPath)`: a path is ignored when the last matching
    /// rule says so.
    fn ignores(&self, relative_path: &str) -> bool {
        let mut ignored = false;
        for (glob, negated) in &self.rules {
            let matcher = glob.compile_matcher();
            // Directory entries carry a trailing slash, which a rule anchored
            // at the directory itself matches through its prefix.
            let candidate = relative_path.trim_end_matches('/');
            let is_match = matcher.is_match(candidate) || matcher.is_match(format!("{candidate}/"));
            if is_match {
                ignored = !*negated;
            }
        }
        ignored
    }
}

/// pi's `addIgnoreRules` (`skills.ts:47-65`).
fn add_ignore_rules(matcher: &mut IgnoreMatcher, dir: &Path, root_dir: &Path) {
    let relative_dir = dir.strip_prefix(root_dir).unwrap_or(Path::new(""));
    let prefix = if relative_dir.as_os_str().is_empty() {
        String::new()
    } else {
        format!("{}/", relative_dir.to_string_lossy().replace('\\', "/"))
    };
    for filename in IGNORE_FILE_NAMES {
        let ignore_path = dir.join(filename);
        let Ok(content) = std::fs::read_to_string(&ignore_path) else {
            continue;
        };
        let patterns: Vec<String> = content
            .split(['\r', '\n'])
            .filter_map(|line| prefix_ignore_pattern(line, &prefix))
            .collect();
        if !patterns.is_empty() {
            matcher.add_rules(&patterns);
        }
    }
}

/// pi's `createSkillSourceInfo` (`skills.ts:136-158`).
fn skill_scope(source: &str) -> Scope {
    match source {
        "user" => Scope::User,
        "project" => Scope::Project,
        // "path" and any other source id have no scope of their own
        // (`skills.ts:150-157`).
        _ => Scope::Temporary,
    }
}

/// pi's `loadSkillsFromDir` (`skills.ts:168-171`).
pub fn load_skills_from_dir(options: &LoadSkillsFromDirOptions) -> LoadSkillsResult {
    load_skills_from_dir_internal(&options.dir, &options.source, true, None, None)
}

/// pi's `loadSkillsFromDirInternal` (`skills.ts:173-275`).
fn load_skills_from_dir_internal(
    dir: &Path,
    source: &str,
    include_root_files: bool,
    ignore_matcher: Option<&IgnoreMatcher>,
    root_dir: Option<&Path>,
) -> LoadSkillsResult {
    let mut skills: Vec<Skill> = Vec::new();
    let mut diagnostics: Vec<ResourceDiagnostic> = Vec::new();

    if !dir.exists() {
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    }

    let root = root_dir.unwrap_or(dir).to_path_buf();
    // pi's shared matcher accumulates every directory's rules
    // (`skills.ts:188-189`); a clone per level is equivalent because
    // `prefixIgnorePattern` anchors every rule at its own directory, so a
    // sibling's rules can never match outside that sibling.
    let mut matcher = match ignore_matcher {
        Some(matcher) => matcher.clone(),
        None => IgnoreMatcher::default(),
    };
    add_ignore_rules(&mut matcher, dir, &root);
    let matcher = &matcher;

    let Ok(entries) = std::fs::read_dir(dir) else {
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    };
    let mut entries: Vec<std::fs::DirEntry> = entries.filter_map(std::result::Result::ok).collect();
    // `readdirSync` order, which the scan relies on for "first wins".
    entries.sort_by_key(std::fs::DirEntry::file_name);

    // A directory with a SKILL.md is a skill root: load it and stop.
    for entry in &entries {
        if entry.file_name() != "SKILL.md" {
            continue;
        }
        let full_path = entry.path();
        let Ok(metadata) = std::fs::metadata(&full_path) else {
            continue;
        };
        let rel_path = full_path
            .strip_prefix(&root)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if !metadata.is_file() || matcher.ignores(&rel_path) {
            continue;
        }
        let result = load_skill_from_file(&full_path, source);
        if let Some(skill) = result.skill {
            skills.push(skill);
        }
        diagnostics.extend(result.diagnostics);
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    }

    for entry in &entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }

        let full_path = entry.path();
        let Ok(metadata) = std::fs::metadata(&full_path) else {
            // Broken symlink, skip it.
            continue;
        };
        let is_directory = metadata.is_dir();
        let is_file = metadata.is_file();

        let rel_path = full_path
            .strip_prefix(&root)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let ignore_path = if is_directory {
            format!("{rel_path}/")
        } else {
            rel_path.clone()
        };
        if matcher.ignores(&ignore_path) {
            continue;
        }

        if is_directory {
            let sub_result = load_skills_from_dir_internal(
                &full_path,
                source,
                false,
                Some(matcher),
                Some(&root),
            );
            skills.extend(sub_result.skills);
            diagnostics.extend(sub_result.diagnostics);
            continue;
        }

        if !is_file || !include_root_files || !name.ends_with(".md") {
            continue;
        }

        let result = load_skill_from_file(&full_path, source);
        if let Some(skill) = result.skill {
            skills.push(skill);
        }
        diagnostics.extend(result.diagnostics);
    }

    LoadSkillsResult {
        skills,
        diagnostics,
    }
}

/// pi's `loadSkillFromFile` (`skills.ts:277-345`).
fn load_skill_from_file(file_path: &Path, source: &str) -> LoadedSkill {
    let mut diagnostics: Vec<ResourceDiagnostic> = Vec::new();
    let is_declared_skill = file_path.file_name().is_some_and(|name| name == "SKILL.md");

    let raw_content = match std::fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Warning,
                message: format!("failed to read skill file: {error}"),
                path: file_path.to_path_buf(),
                collision: None,
            });
            return LoadedSkill {
                skill: None,
                diagnostics,
            };
        }
    };

    let parsed = match parse_frontmatter(&raw_content) {
        Ok(parsed) => parsed,
        Err(message) => {
            if is_declared_skill {
                diagnostics.push(ResourceDiagnostic {
                    kind: DiagnosticKind::Warning,
                    message,
                    path: file_path.to_path_buf(),
                    collision: None,
                });
            }
            return LoadedSkill {
                skill: None,
                diagnostics,
            };
        }
    };

    let description = parsed.get("description").and_then(FrontmatterValue::as_str);
    let has_description = description.is_some_and(|description| !description.trim().is_empty());
    if !is_declared_skill && !has_description {
        return LoadedSkill {
            skill: None,
            diagnostics,
        };
    }

    let skill_dir = file_path.parent().unwrap_or(Path::new("/")).to_path_buf();
    let parent_dir_name = skill_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Validate description.
    for error in validate_description(description) {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: error.clone(),
            path: file_path.to_path_buf(),
            collision: None,
        });
    }

    // Use name from frontmatter, or fall back to parent directory name.
    let frontmatter_name = parsed.get("name").and_then(FrontmatterValue::as_str);
    let name = frontmatter_name.unwrap_or(&parent_dir_name);

    // Validate name.
    for error in validate_name(name) {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: error.clone(),
            path: file_path.to_path_buf(),
            collision: None,
        });
    }

    // Still load the skill even with warnings, unless description is missing
    // or empty (`skills.ts:329-332`).
    let Some(description) = description else {
        return LoadedSkill {
            skill: None,
            diagnostics,
        };
    };

    LoadedSkill {
        skill: Some(Skill {
            name: name.to_string(),
            description: description.to_string(),
            file_path: file_path.to_path_buf(),
            base_dir: skill_dir,
            source: source.to_string(),
            scope: skill_scope(source),
            disable_model_invocation: matches!(
                parsed.get("disable-model-invocation"),
                Some(FrontmatterValue::Boolean(true))
            ),
        }),
        diagnostics,
    }
}

/// pi's `formatSkillsForPrompt` (`skills.ts:358-388`), byte-identical.
///
/// `file_read_tool` names the tool that loads skill files; `Indirect` names
/// none, for a reader that is reachable only through another tool.
pub fn format_skills_for_prompt(skills: &[Skill], file_read_tool: FileReadTool) -> String {
    let visible_skills: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible_skills.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        match file_read_tool {
            FileReadTool::Read => "Use the read tool to load a skill's file when the task matches its description.",
            FileReadTool::Bash => "Use bash to load a skill's file when the task matches its description.",
            FileReadTool::Indirect => "Load a skill's file when the task matches its description.",
        }
        .to_string(),
        "Resolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls."
            .to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];

    for skill in visible_skills {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.display().to_string())
        ));
        lines.push("  </skill>".to_string());
    }

    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

/// pi's `fileReadTool` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileReadTool {
    Read,
    Bash,
    Indirect,
}

/// pi's `escapeXml` (`skills.ts:390-397`).
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// pi's `LoadSkillsOptions` (`skills.ts:399-408`).
pub struct LoadSkillsOptions {
    /// Working directory for project-local skills.
    pub cwd: PathBuf,
    /// Agent config directory for global skills.
    pub agent_dir: PathBuf,
    /// Explicit skill paths (files or directories).
    pub skill_paths: Vec<PathBuf>,
    /// Include default skills directories.
    pub include_defaults: bool,
}

/// pi's `getResourcePathKey` (`core/source-info.ts:58-60`): synthetic paths
/// as-is, file paths through their canonical form.
fn resource_path_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// pi's `loadSkills` (`skills.ts:414-505`).
// A verbatim 1:1 port of the TS function; keeping it in one piece makes the
// side-by-side review against the source possible.
#[allow(clippy::too_many_lines)]
pub fn load_skills(options: &LoadSkillsOptions) -> LoadSkillsResult {
    let resolved_cwd = options
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| options.cwd.clone());
    let resolved_agent_dir = options
        .agent_dir
        .canonicalize()
        .unwrap_or_else(|_| options.agent_dir.clone());

    let mut skill_map: BTreeMap<String, Skill> = BTreeMap::new();
    let mut real_path_set: HashSet<PathBuf> = HashSet::new();
    let mut all_diagnostics: Vec<ResourceDiagnostic> = Vec::new();
    let mut collision_diagnostics: Vec<ResourceDiagnostic> = Vec::new();

    let add_skills = |result: LoadSkillsResult,
                      skill_map: &mut BTreeMap<String, Skill>,
                      real_path_set: &mut HashSet<PathBuf>,
                      all_diagnostics: &mut Vec<ResourceDiagnostic>,
                      collision_diagnostics: &mut Vec<ResourceDiagnostic>| {
        all_diagnostics.extend(result.diagnostics);
        for skill in result.skills {
            // Resolve symlinks to detect duplicate files.
            let real_path = resource_path_key(&skill.file_path);
            // Skip silently if we've already loaded this exact file (via
            // symlink).
            if real_path_set.contains(&real_path) {
                continue;
            }
            if let Some(existing) = skill_map.get(&skill.name) {
                collision_diagnostics.push(ResourceDiagnostic {
                    kind: DiagnosticKind::Collision,
                    message: format!("name \"{}\" collision", skill.name),
                    path: skill.file_path.clone(),
                    collision: Some(json!({
                        "resourceType": "skill",
                        "name": skill.name,
                        "winnerPath": existing.file_path.display().to_string(),
                        "loserPath": skill.file_path.display().to_string(),
                    })),
                });
            } else {
                real_path_set.insert(real_path);
                skill_map.insert(skill.name.clone(), skill);
            }
        }
    };

    if options.include_defaults {
        let result = load_skills_from_dir_internal(
            &resolved_agent_dir.join("skills"),
            "user",
            true,
            None,
            None,
        );
        add_skills(
            result,
            &mut skill_map,
            &mut real_path_set,
            &mut all_diagnostics,
            &mut collision_diagnostics,
        );
        let result = load_skills_from_dir_internal(
            &resolved_cwd.join(CONFIG_DIR_NAME).join("skills"),
            "project",
            true,
            None,
            None,
        );
        add_skills(
            result,
            &mut skill_map,
            &mut real_path_set,
            &mut all_diagnostics,
            &mut collision_diagnostics,
        );
    }

    let user_skills_dir = resolved_agent_dir.join("skills");
    let project_skills_dir = resolved_cwd.join(CONFIG_DIR_NAME).join("skills");

    let get_source = |resolved_path: &Path| -> &'static str {
        if !options.include_defaults {
            if resolved_path.starts_with(&user_skills_dir) {
                return "user";
            }
            if resolved_path.starts_with(&project_skills_dir) {
                return "project";
            }
        }
        "path"
    };

    for raw_path in &options.skill_paths {
        let resolved_path = resolve_skill_path(raw_path, &resolved_cwd);
        if !resolved_path.exists() {
            all_diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Warning,
                message: "skill path does not exist".to_string(),
                path: resolved_path,
                collision: None,
            });
            continue;
        }

        match std::fs::metadata(&resolved_path) {
            Ok(metadata) => {
                let source = get_source(&resolved_path);
                if metadata.is_dir() {
                    let result =
                        load_skills_from_dir_internal(&resolved_path, source, true, None, None);
                    add_skills(
                        result,
                        &mut skill_map,
                        &mut real_path_set,
                        &mut all_diagnostics,
                        &mut collision_diagnostics,
                    );
                } else if metadata.is_file()
                    && resolved_path.extension().is_some_and(|ext| ext == "md")
                {
                    let result = load_skill_from_file(&resolved_path, source);
                    match result.skill {
                        Some(skill) => {
                            add_skills(
                                LoadSkillsResult {
                                    skills: vec![skill],
                                    diagnostics: result.diagnostics,
                                },
                                &mut skill_map,
                                &mut real_path_set,
                                &mut all_diagnostics,
                                &mut collision_diagnostics,
                            );
                        }
                        None => all_diagnostics.extend(result.diagnostics),
                    }
                } else {
                    all_diagnostics.push(ResourceDiagnostic {
                        kind: DiagnosticKind::Warning,
                        message: "skill path is not a markdown file".to_string(),
                        path: resolved_path,
                        collision: None,
                    });
                }
            }
            Err(error) => {
                all_diagnostics.push(ResourceDiagnostic {
                    kind: DiagnosticKind::Warning,
                    message: format!("failed to read skill path: {error}"),
                    path: resolved_path,
                    collision: None,
                });
            }
        }
    }

    LoadSkillsResult {
        skills: skill_map.into_values().collect(),
        diagnostics: [all_diagnostics, collision_diagnostics].concat(),
    }
}

/// pi's `resolvePath(rawPath, resolvedCwd, { trim: true })`
/// (`utils/paths.ts:145-151`): tilde expansion, then resolve against cwd.
fn resolve_skill_path(path: &Path, cwd: &Path) -> PathBuf {
    let text = path.to_string_lossy().trim().to_string();
    let expanded = if text == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(&text))
    } else if let Some(rest) = text.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(&text),
        }
    } else {
        PathBuf::from(&text)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}
