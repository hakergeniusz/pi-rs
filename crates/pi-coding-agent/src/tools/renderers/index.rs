//! Built-in tool renderers — the interactive mode's per-tool presentation.
//!
//! Ground truth: pi-mono
//! `packages/coding-agent/src/core/tools/renderers/` (`index.ts` plus the
//! per-tool `renderers/*.ts`) and `core/tools/render-utils.ts`.
//!
//! Upstream renderers return Components that are mutated across frames; this
//! port's renderers are pure functions from the row state to styled text,
//! which the [`ToolExecutionComponent`](crate::modes::interactive::components::tool_execution::ToolExecutionComponent)
//! places in its call/result `Text` slots. The rendered bytes match: every
//! helper here is a direct port of its TS namesake.

use pi_agent::types::AgentToolResult;
use pi_tui::theme::Theme;
use serde_json::Value;

/// Context for `renderCall` (`ToolRenderContext` reduced to what the built-in
/// renderers read).
pub struct CallContext<'a> {
    pub theme: &'a Theme,
    pub cwd: &'a str,
    /// pi's `expanded` (ctrl+o).
    pub expanded: bool,
}

/// Context for `renderResult`.
pub struct ResultContext<'a> {
    pub theme: &'a Theme,
    pub cwd: &'a str,
    pub expanded: bool,
    pub is_error: bool,
    /// `toolTimerText` output (`Elapsed 4.2s` / `Took 4.2s`), when the row
    /// has a clock.
    pub timer_text: Option<String>,
}

/// A tool's presentation functions. `ToolRenderers`
/// (`core/extensions/types.ts`), reduced to the built-in shape.
#[derive(Clone, Copy)]
pub struct ToolRenderers {
    /// pi's `renderCall`: the line shown while the tool runs.
    pub render_call: fn(args: Option<&Value>, context: &CallContext) -> String,
    /// pi's `renderResult`: the body shown for a result.
    pub render_result:
        fn(args: Option<&Value>, result: &AgentToolResult, context: &ResultContext) -> String,
}

/// pi's collapsed preview budget (`render-utils.ts:85`).
const COLLAPSED_ARGS_CHARS: usize = 100;

/// `formatToolCallWithArgs` (`render-utils.ts:92-110`): the generic fallback
/// call line — title, then `key=value` pairs collapsed to 100 chars, or one
/// `key: value` line each when expanded.
pub fn format_tool_call_with_args(
    title: &str,
    args: Option<&Value>,
    theme: &Theme,
    expanded: bool,
) -> String {
    let header = theme.fg(pi_tui::theme::ThemeColor::ToolTitle, &bold(title));
    let Some(args) = args else {
        return header;
    };
    let entries: Vec<(String, &Value)> = match args {
        Value::Object(map) => map
            .iter()
            .map(|(key, value)| (key.clone(), value))
            .collect(),
        Value::Null => return header,
        other => vec![("args".to_string(), other)],
    };
    if entries.is_empty() {
        return header;
    }
    if expanded {
        let lines = entries
            .iter()
            .map(|(key, value)| {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => {
                        serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string())
                    }
                };
                let body = text.replace('\r', "").replace('\n', "\n    ");
                format!("  {key}: {}", replace_tabs(&body))
            })
            .collect::<Vec<_>>()
            .join("\n");
        return format!(
            "{header}\n{}",
            theme.fg(pi_tui::theme::ThemeColor::Muted, &lines)
        );
    }
    let pairs = entries
        .iter()
        .map(|(key, value)| {
            let rendered = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
            format!("{key}={rendered}")
        })
        .collect::<Vec<_>>()
        .join(" ");
    let preview = if pairs.chars().count() > COLLAPSED_ARGS_CHARS {
        let head: String = pairs.chars().take(COLLAPSED_ARGS_CHARS - 3).collect();
        format!("{head}...")
    } else {
        pairs
    };
    format!(
        "{header} {}",
        theme.fg(pi_tui::theme::ThemeColor::Muted, &preview)
    )
}

/// `replaceTabs` (`render-utils.ts:45-47`).
pub fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

/// `invalidArgText` (`render-utils.ts:166-168`).
pub fn invalid_arg_text(theme: &Theme) -> String {
    theme.fg(pi_tui::theme::ThemeColor::Error, "[invalid arg]")
}

/// `renderToolPath` (`render-utils.ts:170-181`): styled, shortened path.
/// Hyperlinks (upstream's `linkPath`) are deferred with the capability work.
pub fn render_tool_path(
    raw_path: Option<&str>,
    theme: &Theme,
    empty_fallback: Option<&str>,
) -> String {
    let Some(value) = raw_path else {
        return invalid_arg_text(theme);
    };
    let value = if value.is_empty() {
        empty_fallback
    } else {
        Some(value)
    };
    match value {
        None => theme.fg(pi_tui::theme::ThemeColor::ToolOutput, "..."),
        Some(value) => theme.fg(
            pi_tui::theme::ThemeColor::Accent,
            &crate::tools::render_utils::shorten_path(value),
        ),
    }
}

/// `getLanguageFromPath` (`theme.ts:993-1063`), reduced to the extensions the
/// port's tools actually meet. Upstream maps dozens more; the fallback is the
/// same: no language.
pub fn get_language_from_path(file_path: &str) -> Option<&'static str> {
    let extension = file_path.rsplit('.').next()?.to_ascii_lowercase();
    let language = match extension.as_str() {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "lua" => "lua",
        "perl" => "perl",
        _ => return None,
    };
    Some(language)
}

/// Renderers for every built-in tool, keyed by tool name — pi's
/// `createAllToolRenderers()`. The skipped Windows-only `powershell` variant
/// renders as the shell renderer with `PS>` for symmetry.
pub fn create_all_tool_renderers() -> Vec<(&'static str, ToolRenderers)> {
    vec![
        ("read", super::read::READ_RENDERERS),
        ("bash", super::bash::create_shell_renderers("$")),
        ("powershell", super::bash::create_shell_renderers("PS>")),
        ("edit", super::edit::EDIT_RENDERERS),
        ("write", super::write::WRITE_RENDERERS),
        ("grep", super::grep::GREP_RENDERERS),
        ("find", super::find::FIND_RENDERERS),
        ("ls", super::ls::LS_RENDERERS),
    ]
}

/// The renderers for one tool name, or pi's `undefined`.
pub fn renderers_for(tool_name: &str) -> Option<ToolRenderers> {
    create_all_tool_renderers()
        .into_iter()
        .find(|(name, _)| *name == tool_name)
        .map(|(_, renderers)| renderers)
}

/// pi's `withBuiltInRenderers` (`renderers/index.ts:60-71`): merge built-in
/// renderers into a tool that does not supply its own. This port's tool
/// definitions all carry renderers, so the merge is the lookup itself.
pub fn with_built_in_renderers(tool_name: &str) -> Option<ToolRenderers> {
    renderers_for(tool_name)
}

/// `theme.bold` (`theme.ts:314-316`): bold + reset, inline to keep this
/// module free of higher-layer imports.
fn bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[22m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapsed_args_fit_the_budget() {
        let theme = test_theme();
        let args = serde_json::json!({ "path": "/tmp/some-file.txt", "limit": 10 });
        let line = format_tool_call_with_args("read", Some(&args), &theme, false);
        let plain = pi_tui::ansi::strip_terminal_sequences(&line);
        assert!(plain.contains("read"), "{plain}");
        assert!(plain.contains("path="), "{plain}");

        let long = serde_json::json!({ "content": "x".repeat(500) });
        let line = format_tool_call_with_args("write", Some(&long), &theme, false);
        let plain = pi_tui::ansi::strip_terminal_sequences(&line);
        assert!(
            plain.contains("..."),
            "collapsed preview must cut at 100 chars: {plain}"
        );
    }

    #[test]
    fn expanded_args_use_key_value_lines() {
        let theme = test_theme();
        let args = serde_json::json!({ "command": "echo\nhi" });
        let line = format_tool_call_with_args("bash", Some(&args), &theme, true);
        let plain = pi_tui::ansi::strip_terminal_sequences(&line);
        assert!(plain.contains("  command: echo"), "{plain}");
        assert!(plain.contains("\n    hi"), "{plain}");
    }

    #[test]
    fn language_table_matches_upstream_core() {
        assert_eq!(get_language_from_path("foo.rs"), Some("rust"));
        assert_eq!(get_language_from_path("a/b/c.d.ts"), Some("typescript"));
        assert_eq!(get_language_from_path("no-extension"), None);
    }

    fn test_theme() -> Theme {
        crate::modes::interactive::theme::init_theme(Some("dark"));
        let theme = crate::modes::interactive::theme::theme();
        (*theme).clone()
    }
}
