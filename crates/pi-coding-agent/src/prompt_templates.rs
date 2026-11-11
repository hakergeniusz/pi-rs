//! Command prompt templates.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/prompt-templates.ts`
//! plus `utils/frontmatter.ts` (re-exported from [`crate::skills`]).
//!
//! Templates are `.md` files with optional frontmatter
//! (`description`, `argument-hint`); arguments are bash-style parsed
//! ([`parse_command_args`]) and substituted ([`substitute_args`]): `$1..$N`,
//! `$@` / `$ARGUMENTS`, `${N:-default}`, `${@:-default}`, and the bash-style
//! slices `${@:N}` / `${@:N:L}`.

use std::path::{Path, PathBuf};

use crate::skills::{DiagnosticKind, ResourceDiagnostic, parse_frontmatter};

/// pi's `PromptTemplate` (`prompt-templates.ts:12-19`).
#[derive(Debug, Clone, PartialEq)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub content: String,
    /// pi's `sourceInfo.source`, the identifier the caller gave this scan.
    pub source: String,
    /// pi's `sourceInfo.scope`.
    pub scope: Scope,
    /// Absolute path to the template file.
    pub file_path: PathBuf,
}

/// pi's `SourceScope`.
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

/// pi's `parseCommandArgs` (`prompt-templates.ts:25-77`): parse command
/// arguments respecting quoted strings (bash-style).
///
/// Quotes only open at the start of an argument, so apostrophes inside words
/// stay literal; a backslash before the open quote's character keeps the
/// quote as text; an unclosed quote re-reads its quote character as literal
/// text (`prompt-templates.ts:63-69`).
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let chars: Vec<char> = args_string.chars().collect();
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    // Tracks whether a token has started, so quoted empty strings ("" or '')
    // are kept as arguments.
    let mut has_token = false;
    let mut in_quote: Option<char> = None;
    // Where the open quote started, so an unclosed quote can be re-read as
    // literal text.
    let mut quote_start = 0usize;

    let mut i = 0usize;
    while i < chars.len() {
        let character = chars[i];
        if let Some(quote) = in_quote {
            if character == '\\' && chars.get(i + 1) == Some(&quote) {
                // A backslash before the open quote's character keeps it as
                // text: "say \"hi\"" gives say "hi".
                current.push(quote);
                i += 1;
            } else if character == quote {
                in_quote = None;
            } else {
                current.push(character);
            }
        } else if (character == '"' || character == '\'') && !has_token {
            in_quote = Some(character);
            quote_start = i;
            has_token = true;
        } else if character.is_whitespace() {
            if has_token {
                args.push(std::mem::take(&mut current));
                has_token = false;
            }
        } else {
            current.push(character);
            has_token = true;
        }

        if in_quote.is_some() && i == chars.len() - 1 {
            // Unclosed quote: keep the quote character as text and parse the
            // rest normally. `current` was empty when the quote opened,
            // because quotes only open a new argument.
            in_quote = None;
            current = chars[quote_start].to_string();
            i = quote_start;
        }
        i += 1;
    }

    if has_token {
        args.push(current);
    }
    args
}

/// pi's `substituteArgs` (`prompt-templates.ts:92-124`).
///
/// Replacement happens on the template string only. Argument and default
/// values containing patterns like `$1`, `$@`, or `$ARGUMENTS` are NOT
/// recursively substituted.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let all_args = args.join(" ");
    let pattern = regex::Regex::new(
        r"\$\{(\d+|ARGUMENTS|@):-([^}]*)\}|\$\{@:(\d+)(?::(\d+))?\}|\$(ARGUMENTS|@|\d+)",
    )
    .expect("template substitution pattern parses");
    pattern
        .replace_all(content, |captures: &regex::Captures| {
            let default_target = captures.get(1);
            if let Some(default_target) = default_target {
                let value = match default_target.as_str() {
                    "@" | "ARGUMENTS" => Some(all_args.clone()),
                    index => index
                        .parse::<usize>()
                        .expect("digits only")
                        .checked_sub(1)
                        .and_then(|position| args.get(position))
                        .cloned(),
                };
                return match value {
                    Some(value) if !value.is_empty() => value,
                    _ => captures
                        .get(2)
                        .map(|group| group.as_str().to_string())
                        .unwrap_or_default(),
                };
            }
            if let Some(slice_start) = captures.get(3) {
                // Convert to 0-indexed (user provides 1-indexed); treat 0 as 1
                // (bash convention: args start at 1).
                let start = slice_start
                    .as_str()
                    .parse::<usize>()
                    .expect("digits only")
                    .saturating_sub(1);
                return match captures.get(4) {
                    Some(slice_length) => {
                        let length = slice_length.as_str().parse::<usize>().expect("digits only");
                        args.iter()
                            .skip(start)
                            .take(length)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" ")
                    }
                    None => args
                        .iter()
                        .skip(start)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" "),
                };
            }
            match captures.get(5).map(|group| group.as_str()) {
                Some("ARGUMENTS" | "@") => all_args.clone(),
                Some(simple) => {
                    let index = simple.parse::<usize>().expect("digits only");
                    index
                        .checked_sub(1)
                        .and_then(|position| args.get(position))
                        .cloned()
                        .unwrap_or_default()
                }
                None => String::new(),
            }
        })
        .into_owned()
}

/// pi's `loadTemplateFromFile` (`prompt-templates.ts:126-175`).
fn load_template_from_file(
    file_path: &Path,
    source: &str,
    scope: Scope,
) -> (Option<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut diagnostics: Vec<ResourceDiagnostic> = Vec::new();
    let raw_content = match std::fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(error) => {
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Warning,
                message: format!("failed to read prompt template file: {error}"),
                path: file_path.to_path_buf(),
                collision: None,
            });
            return (None, diagnostics);
        }
    };

    let parsed = match parse_frontmatter(&raw_content) {
        Ok(parsed) => parsed,
        Err(message) => {
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Warning,
                message,
                path: file_path.to_path_buf(),
                collision: None,
            });
            return (None, diagnostics);
        }
    };

    let name = file_path
        .file_name()
        .map(|name| {
            let name = name.to_string_lossy();
            name.strip_suffix(".md").unwrap_or(&name).to_string()
        })
        .unwrap_or_default();

    // Get description from frontmatter or first non-empty line
    // (`prompt-templates.ts:152-161`).
    let mut description = parsed
        .get("description")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    if description.is_empty() {
        let first_line = parsed.body.split('\n').find(|line| !line.trim().is_empty());
        if let Some(first_line) = first_line {
            // Truncate if too long: 60 characters, then "...".
            let mut chars = first_line.chars();
            let truncated: String = chars.by_ref().take(60).collect();
            description = truncated;
            if chars.next().is_some() {
                description.push_str("...");
            }
        }
    }

    let argument_hint = parsed
        .get("argument-hint")
        .and_then(|value| value.as_str())
        .filter(|hint| !hint.is_empty())
        .map(str::to_string);
    (
        Some(PromptTemplate {
            name,
            description,
            argument_hint,
            content: parsed.body,
            source: source.to_string(),
            scope,
            file_path: file_path.to_path_buf(),
        }),
        diagnostics,
    )
}

/// pi's `loadTemplatesFromDir` (`prompt-templates.ts:180-219`): scan a
/// directory for `.md` files (non-recursive) and load them as prompt
/// templates.
fn load_templates_from_dir(
    dir: &Path,
    source: &str,
    scope: Scope,
) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut templates: Vec<PromptTemplate> = Vec::new();
    let mut diagnostics: Vec<ResourceDiagnostic> = Vec::new();

    if !dir.exists() {
        return (templates, diagnostics);
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return (templates, diagnostics);
    };
    let mut entries: Vec<std::fs::DirEntry> = entries.filter_map(std::result::Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let full_path = entry.path();
        // For symlinks, check if they point to a file; broken symlinks are
        // skipped (`prompt-templates.ts:196-204`).
        let Ok(metadata) = std::fs::metadata(&full_path) else {
            continue;
        };
        let is_file = metadata.is_file();
        let name = entry.file_name().to_string_lossy().into_owned();
        // The TS `name.endsWith(".md")` is case-sensitive; `.MD` is not a
        // template file, so keep the byte comparison (`prompt-templates.ts`).
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        if is_file && name.ends_with(".md") {
            let (template, template_diagnostics) =
                load_template_from_file(&full_path, source, scope);
            if let Some(template) = template {
                templates.push(template);
            }
            diagnostics.extend(template_diagnostics);
        }
    }

    (templates, diagnostics)
}

/// pi's `LoadPromptTemplatesOptions` (`prompt-templates.ts:221-230`).
pub struct LoadPromptTemplatesOptions {
    /// Working directory for project-local templates.
    pub cwd: PathBuf,
    /// Agent config directory for global templates.
    pub agent_dir: PathBuf,
    /// Explicit prompt template paths (files or directories).
    pub prompt_paths: Vec<PathBuf>,
    /// Include default prompt directories.
    pub include_defaults: bool,
}

/// pi's `LoadPromptTemplatesResult`.
#[derive(Debug, Clone, Default)]
pub struct LoadPromptTemplatesResult {
    pub templates: Vec<PromptTemplate>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

/// pi's `loadPromptTemplates` (`prompt-templates.ts:243-319`).
///
/// Loads from global `agentDir/prompts/`, project
/// `cwd/{CONFIG_DIR_NAME}/prompts/`, and explicit prompt paths.
pub fn load_prompt_templates(options: &LoadPromptTemplatesOptions) -> LoadPromptTemplatesResult {
    let resolved_cwd = options
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| options.cwd.clone());
    let resolved_agent_dir = options
        .agent_dir
        .canonicalize()
        .unwrap_or_else(|_| options.agent_dir.clone());

    let mut templates: Vec<PromptTemplate> = Vec::new();
    let mut diagnostics: Vec<ResourceDiagnostic> = Vec::new();

    let global_prompts_dir = resolved_agent_dir.join("prompts");
    let project_prompts_dir = resolved_cwd
        .join(crate::tools::CONFIG_DIR_NAME)
        .join("prompts");

    let add_result = |result: (Vec<PromptTemplate>, Vec<ResourceDiagnostic>),
                      templates: &mut Vec<PromptTemplate>,
                      diagnostics: &mut Vec<ResourceDiagnostic>| {
        templates.extend(result.0);
        diagnostics.extend(result.1);
    };

    if options.include_defaults {
        add_result(
            load_templates_from_dir(&global_prompts_dir, "local", Scope::User),
            &mut templates,
            &mut diagnostics,
        );
        add_result(
            load_templates_from_dir(&project_prompts_dir, "local", Scope::Project),
            &mut templates,
            &mut diagnostics,
        );
    }

    // 3. Load explicit prompt paths (`prompt-templates.ts:294-316`).
    for raw_path in &options.prompt_paths {
        let resolved_path = resolve_template_path(raw_path, &resolved_cwd);
        if !resolved_path.exists() {
            continue;
        }
        match std::fs::metadata(&resolved_path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    add_result(
                        load_templates_from_dir(&resolved_path, "local", Scope::Temporary),
                        &mut templates,
                        &mut diagnostics,
                    );
                } else if metadata.is_file() && resolved_path.to_string_lossy().ends_with(".md") {
                    let (template, template_diagnostics) =
                        load_template_from_file(&resolved_path, "local", Scope::Temporary);
                    if let Some(template) = template {
                        templates.push(template);
                    }
                    diagnostics.extend(template_diagnostics);
                }
            }
            Err(error) => {
                diagnostics.push(ResourceDiagnostic {
                    kind: DiagnosticKind::Warning,
                    message: format!("failed to read prompt template path: {error}"),
                    path: resolved_path,
                    collision: None,
                });
            }
        }
    }

    LoadPromptTemplatesResult {
        templates,
        diagnostics,
    }
}

/// pi's `resolvePath(rawPath, resolvedCwd, { trim: true })`.
fn resolve_template_path(path: &Path, cwd: &Path) -> PathBuf {
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

/// pi's `expandPromptTemplate` (`prompt-templates.ts:325-341`): expand a
/// prompt template if it matches a template name. Returns the expanded
/// content or the original text if not a template.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }

    // `/^\/([^\s]+)(?:\s+([\s\S]*))?$/`
    let rest = &text[1..];
    let Some(space_index) = rest.find(|character: char| character.is_whitespace()) else {
        let template = templates.iter().find(|template| template.name == rest);
        return match template {
            Some(template) => substitute_args(&template.content, &[]),
            None => text.to_string(),
        };
    };
    let template_name = &rest[..space_index];
    let args_string = rest[space_index..].trim_start();

    let template = templates
        .iter()
        .find(|template| template.name == template_name);
    match template {
        Some(template) => {
            let args = parse_command_args(args_string);
            substitute_args(&template.content, &args)
        }
        None => text.to_string(),
    }
}
