//! System prompt construction and project context loading.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/core/system-prompt.ts`,
//! byte-identical in section order and wording.
//!
//! The prompt is an ordered set of named sections (`buildSystemPromptSections`,
//! `system-prompt.ts:128-192`): an untagged `preamble` and, for every other
//! section, `<name>\n…\n</name>`. `build_system_prompt` renders the text
//! exactly as the transcript's system message replays it — content followed by
//! the sections' values joined with blank lines, pi-ai's
//! `getSystemMessageText` (`pi-ai` `utils/text.ts`).
//!
//! Section order is insertion order, which is what the TS object keeps, so
//! [`SystemPromptSections`] is a `Vec` of pairs, not a sorted map.
//!
//! The `mcp_servers` section is rendered by [`mcp_section`] from a host-supplied
//! snapshot (`extensions/mcp/index.ts:156-222`) and inserted as an ordinary
//! custom section after `cwd`.

/// The `mcp_servers` system-prompt section (`extensions/mcp/index.ts:156-222`).
pub mod mcp_section;

use std::collections::HashMap;

use crate::skills::{FileReadTool, Skill, format_skills_for_prompt};

/// pi's `BuildSystemPromptOptions` (`system-prompt.ts:9-37`).
#[derive(Debug, Clone, Default)]
pub struct BuildSystemPromptOptions {
    /// Custom system prompt (replaces the default prefix).
    pub custom_prompt: Option<String>,
    /// Exact full prompt replacement set by a `before_agent_start` handler.
    pub force_system_prompt: Option<String>,
    /// Tools to include in prompt. Default: [read, bash, edit, write].
    pub selected_tools: Option<Vec<String>>,
    /// Selected tools whose declarations requests leave out
    /// (`prepareLoadout` hidden declarations). They are reachable only
    /// through another tool, so the tool list and rules leave them out too.
    pub hidden_tools: Vec<String>,
    /// Optional one-line tool snippets keyed by tool name.
    pub tool_snippets: HashMap<String, String>,
    /// Guideline bullets contributed by each tool, keyed by tool name.
    pub tool_guidelines: HashMap<String, Vec<String>>,
    /// Additional guideline bullets appended to the default system prompt
    /// rules.
    pub prompt_guidelines: Vec<String>,
    /// Text appended from user configuration before project context, skills,
    /// and cwd.
    pub append_system_prompt: Option<String>,
    /// Additional XML-wrapped prompt sections keyed by tag name, in the order
    /// the caller gave them.
    pub sections: Vec<(String, String)>,
    /// Working directory.
    pub cwd: String,
    /// Pre-loaded context files.
    pub context_files: Vec<ContextFile>,
    /// Pre-loaded skills.
    pub skills: Vec<Skill>,
}

/// pi's `contextFiles` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    pub path: String,
    pub content: String,
}

impl BuildSystemPromptOptions {
    /// Options with only the working directory set.
    pub fn new(cwd: impl Into<String>) -> BuildSystemPromptOptions {
        BuildSystemPromptOptions {
            cwd: cwd.into(),
            ..BuildSystemPromptOptions::default()
        }
    }
}

/// pi's `NormalizedBuildSystemPromptOptions` (`system-prompt.ts:39-49`): the
/// mutable, collection-complete shape exposed to extensions.
#[derive(Debug, Clone)]
pub struct NormalizedBuildSystemPromptOptions {
    pub custom_prompt: Option<String>,
    pub force_system_prompt: Option<String>,
    pub selected_tools: Vec<String>,
    pub hidden_tools: Vec<String>,
    pub tool_snippets: HashMap<String, String>,
    pub tool_guidelines: HashMap<String, Vec<String>>,
    pub prompt_guidelines: Vec<String>,
    pub append_system_prompt: String,
    pub sections: Vec<(String, String)>,
    pub cwd: String,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
}

/// pi's `SYSTEM_PROMPT_SECTION_NAME` (`system-prompt.ts:58`): a lowercase
/// identifier a tag can carry.
fn is_valid_section_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '_'
            || character == '-'
    })
}

/// pi's `normalizeBuildSystemPromptOptions` (`system-prompt.ts:60-77`).
pub fn normalize_build_system_prompt_options(
    input: &BuildSystemPromptOptions,
) -> NormalizedBuildSystemPromptOptions {
    NormalizedBuildSystemPromptOptions {
        custom_prompt: input.custom_prompt.clone(),
        force_system_prompt: input.force_system_prompt.clone(),
        selected_tools: input.selected_tools.clone().unwrap_or_else(|| {
            ["read", "bash", "edit", "write"]
                .iter()
                .map(std::string::ToString::to_string)
                .collect()
        }),
        hidden_tools: input.hidden_tools.clone(),
        tool_snippets: input.tool_snippets.clone(),
        tool_guidelines: input.tool_guidelines.clone(),
        prompt_guidelines: input.prompt_guidelines.clone(),
        append_system_prompt: input.append_system_prompt.clone().unwrap_or_default(),
        sections: input.sections.clone(),
        cwd: input.cwd.clone(),
        context_files: input.context_files.clone(),
        skills: input.skills.clone(),
    }
}

/// pi's `renderProjectContext` (`system-prompt.ts:79-86`).
fn render_project_context(context_files: &[ContextFile]) -> String {
    let mut lines = vec!["Project-specific instructions and guidelines:".to_string()];
    for file in context_files {
        lines.push(format!(
            "<project_instructions path=\"{}\">\n{}\n</project_instructions>",
            file.path, file.content
        ));
    }
    lines.join("\n\n")
}

/// pi's `buildRules` (`system-prompt.ts:88-125`).
fn build_rules(
    selected_tools: &[String],
    tool_guidelines: &HashMap<String, Vec<String>>,
    prompt_guidelines: &[String],
) -> String {
    let mut rules: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let add_rule =
        |rule: &str, rules: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
            let normalized = rule.trim().to_string();
            if normalized.is_empty() || seen.contains(&normalized) {
                return;
            }
            seen.insert(normalized.clone());
            rules.push(normalized);
        };

    let has_bash = selected_tools.iter().any(|name| name == "bash");
    let has_power_shell = selected_tools.iter().any(|name| name == "powershell");
    let has_grep = selected_tools.iter().any(|name| name == "grep");
    let has_find = selected_tools.iter().any(|name| name == "find");
    let has_ls = selected_tools.iter().any(|name| name == "ls");

    if (has_bash || has_power_shell) && !has_grep && !has_find && !has_ls {
        if has_bash && has_power_shell {
            add_rule(
                "Use bash or PowerShell for file operations like listing, searching, and finding files",
                &mut rules,
                &mut seen,
            );
        } else if has_power_shell {
            add_rule(
                "Use PowerShell for file operations like listing, searching, and finding files",
                &mut rules,
                &mut seen,
            );
        } else {
            add_rule(
                "Use bash for file operations like ls, rg, find",
                &mut rules,
                &mut seen,
            );
        }
    }

    for name in selected_tools {
        if let Some(guidelines) = tool_guidelines.get(name) {
            for rule in guidelines {
                add_rule(rule, &mut rules, &mut seen);
            }
        }
    }
    for rule in prompt_guidelines {
        add_rule(rule, &mut rules, &mut seen);
    }
    add_rule("Be concise in your responses", &mut rules, &mut seen);
    add_rule(
        "Show file paths clearly when working with files",
        &mut rules,
        &mut seen,
    );
    rules
        .iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// pi's `getReadmePath` (`config.ts:469-471`). The package directory a Rust
/// binary has is its executable's directory; `PI_PACKAGE_DIR` overrides it.
fn package_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("PI_PACKAGE_DIR") {
        return std::path::PathBuf::from(dir);
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_default()
}

/// pi's `getReadmePath` (`config.ts:469-471`).
pub fn get_readme_path() -> String {
    package_dir().join("README.md").display().to_string()
}

/// pi's `getDocsPath` (`config.ts:474-476`).
pub fn get_docs_path() -> String {
    package_dir().join("docs").display().to_string()
}

/// pi's `getExamplesPath` (`config.ts:479-481`).
pub fn get_examples_path() -> String {
    package_dir().join("examples").display().to_string()
}

/// pi's `buildSystemPromptSections` (`system-prompt.ts:128-192`).
///
/// Ordered, independently replaceable sections of the structured system
/// prompt. `preamble` is untagged text; every other section is wrapped in a
/// tag of the same name so the model can match later updates to it.
// A verbatim 1:1 port of the TS function; keeping it in one piece makes the
// side-by-side review against the source possible.
#[allow(clippy::too_many_lines)]
pub fn build_system_prompt_sections(
    input: &BuildSystemPromptOptions,
) -> Result<SystemPromptSections, String> {
    // The readers the skills hint may name (`system-prompt.ts:173-177`): a
    // hidden reader is still reachable through another tool, so skills stay
    // but the hint names no tool.
    const READERS: [&str; 2] = ["read", "bash"];
    let options = normalize_build_system_prompt_options(input);
    let NormalizedBuildSystemPromptOptions {
        custom_prompt,
        selected_tools,
        hidden_tools,
        tool_snippets,
        tool_guidelines,
        prompt_guidelines,
        append_system_prompt,
        sections: custom_sections,
        cwd,
        context_files,
        skills,
        ..
    } = options;

    for (name, _) in &custom_sections {
        if !is_valid_section_name(name) || name == "preamble" {
            return Err(format!("Invalid system prompt section name: {name}"));
        }
    }

    let declared_tools: Vec<String> = selected_tools
        .iter()
        .filter(|name| !hidden_tools.contains(name))
        .cloned()
        .collect();
    let mut prompt_sections: Vec<(String, String)> = Vec::new();
    let push = |name: &str, content: String, prompt_sections: &mut Vec<(String, String)>| {
        prompt_sections.push((name.to_string(), content));
    };

    match custom_prompt {
        Some(custom_prompt) if !custom_prompt.is_empty() => {
            push("preamble", custom_prompt, &mut prompt_sections);
        }
        _ => {
            push(
                "preamble",
                "You are a coding assistant running inside pi, a coding agent harness. Help users inspect files, run commands, and modify code."
                    .to_string(),
                &mut prompt_sections,
            );
            let visible_tools: Vec<&String> = declared_tools
                .iter()
                .filter(|name| tool_snippets.contains_key(*name))
                .collect();
            let tools = if visible_tools.is_empty() {
                "(none)".to_string()
            } else {
                visible_tools
                    .iter()
                    .map(|name| {
                        format!(
                            "- {name}: {}",
                            tool_snippets.get(*name).expect("checked above")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            push(
                "tools",
                format!("{tools}\n\nAdditional tools may be available."),
                &mut prompt_sections,
            );
            push(
                "rules",
                build_rules(&declared_tools, &tool_guidelines, &prompt_guidelines),
                &mut prompt_sections,
            );
            push(
                "docs",
                format!(
                    "Pi documentation (read only when the user asks about the harness itself, its SDK, extensions, themes, skills, or TUI):\n\
- Main documentation: {}\n\
- Additional docs: {}\n\
- Examples: {} (extensions, custom tools, SDK)\n\
- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory\n\
- When asked about: extensions (docs/extensions.md), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), packages (docs/packages.md), environment variables (docs/environment-variables.md), MCP servers (docs/mcp.md), codemode scripts and non-LLM models such as classifiers and image models (docs/codemode.md)\n\
- Before implementing harness changes, read the relevant docs and examples in full, following related cross-references.",
                    get_readme_path(),
                    get_docs_path(),
                    get_examples_path()
                ),
                &mut prompt_sections,
            );
        }
    }

    if !append_system_prompt.is_empty() {
        push("addendum", append_system_prompt, &mut prompt_sections);
    }
    if !context_files.is_empty() {
        push(
            "project_context",
            render_project_context(&context_files),
            &mut prompt_sections,
        );
    }
    let skill_file_read_tool = if let Some(tool) = READERS
        .iter()
        .find(|tool| declared_tools.iter().any(|name| name == *tool))
    {
        match *tool {
            "read" => Some(FileReadTool::Read),
            _ => Some(FileReadTool::Bash),
        }
    } else if READERS
        .iter()
        .any(|tool| selected_tools.iter().any(|name| name == tool))
    {
        Some(FileReadTool::Indirect)
    } else {
        None
    };
    if let Some(file_read_tool) = skill_file_read_tool
        && !skills.is_empty()
    {
        let skills_prompt = format_skills_for_prompt(&skills, file_read_tool)
            .trim()
            .to_string();
        if !skills_prompt.is_empty() {
            push("skills", skills_prompt, &mut prompt_sections);
        }
    }
    push("cwd", cwd.replace('\\', "/"), &mut prompt_sections);
    for (name, content) in &custom_sections {
        if !content.is_empty() {
            push(name, content.clone(), &mut prompt_sections);
        }
    }

    let mut sections = SystemPromptSections::default();
    for (name, content) in prompt_sections {
        if name == "preamble" {
            sections.preamble = content;
        } else {
            sections
                .named
                .push((name.clone(), format!("<{name}>\n{content}\n</{name}>")));
        }
    }
    Ok(sections)
}

/// pi's `SystemPromptSections` (`system-prompt.ts:56`): `preamble` is the
/// untagged text, `named` the tagged sections in order. The order is the TS
/// object's insertion order, which `getSystemMessageText` renders.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemPromptSections {
    pub preamble: String,
    pub named: Vec<(String, String)>,
}

impl SystemPromptSections {
    /// The section text by name.
    pub fn get(&self, name: &str) -> Option<&str> {
        if name == "preamble" {
            return Some(&self.preamble);
        }
        self.named
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// pi's `Record<string, string>` shape, for the transcript.
    pub fn to_map(&self) -> Vec<(String, String)> {
        let mut entries = vec![("preamble".to_string(), self.preamble.clone())];
        entries.extend(self.named.iter().cloned());
        entries
    }
}

/// The complete prompt state for `input` — pi's `buildSystemPromptState`
/// (`system-prompt.ts:198-204`). A forced prompt is opaque and lives in
/// `content` with no sections; otherwise `content` is empty and the
/// structured sections carry the prompt.
pub fn build_system_prompt_state(
    input: &BuildSystemPromptOptions,
) -> Result<SystemPromptState, String> {
    if let Some(forced) = &input.force_system_prompt {
        return Ok(SystemPromptState {
            content: forced.clone(),
            sections: None,
        });
    }
    Ok(SystemPromptState {
        content: String::new(),
        sections: Some(build_system_prompt_sections(input)?),
    })
}

/// pi's return shape of `buildSystemPromptState`.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemPromptState {
    pub content: String,
    pub sections: Option<SystemPromptSections>,
}

/// pi's `buildSystemPrompt` (`system-prompt.ts:207-209`): the system prompt
/// text, rendered exactly as the transcript's system message replays it —
/// pi-ai's `getSystemMessageText`: the content, then each section value,
/// empty parts dropped, joined with blank lines.
pub fn build_system_prompt(input: &BuildSystemPromptOptions) -> Result<String, String> {
    let state = build_system_prompt_state(input)?;
    let mut parts: Vec<String> = vec![state.content];
    if let Some(sections) = &state.sections {
        for (_, text) in sections.to_map() {
            parts.push(text);
        }
    }
    Ok(parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// pi's `diffSystemPromptSections` (`system-prompt.ts:216-228`): diff the
/// sections the model currently has (replayed from the transcript, so never
/// null) against the desired ones. Returns a patch, or `None` when nothing
/// changed.
pub fn diff_system_prompt_sections<S: std::hash::BuildHasher>(
    previous: &HashMap<String, Option<String>, S>,
    current: &SystemPromptSections,
) -> Option<Vec<(String, Option<String>)>> {
    let mut patch: Vec<(String, Option<String>)> = Vec::new();
    for (name, text) in current.to_map() {
        let same = previous
            .get(&name)
            .is_some_and(|value| value.as_deref() == Some(text.as_str()));
        if !same {
            patch.push((name, Some(text)));
        }
    }
    for name in previous.keys() {
        if current.get(name).is_none() {
            patch.push((name.clone(), None));
        }
    }
    if patch.is_empty() { None } else { Some(patch) }
}
