//! The HTML export: render a session file as the self-contained viewer page.
//!
//! Ground truth: `packages/coding-agent/src/core/export-html/index.ts` (the
//! whole file, `:1-318`), with the ANSI converter in
//! [`ansi_to_html`] (`export-html/ansi-to-html.ts`) and the tool-renderer
//! seam in [`tool_renderer`] (`export-html/tool-renderer.ts`).
//!
//! # Templates
//!
//! Upstream reads `template.html`, `template.css`, `template.js`, and the
//! three vendored `vendor/*.min.js` files at runtime from the package's
//! `export-html/` directory (`getExportTemplateDir`, `config.ts:454-460`).
//! The port embeds them byte-identically with `include_str!` from
//! `crates/pi-coding-agent/export-html/` — the same crate-root data-file
//! layout as `pi_tui`'s `themes/` and `pi-ai`'s `data/` — so a shipped binary
//! cannot miss its templates. That embedding is a deliberate deviation
//! recorded here; the file contents are upstream's, byte for byte.
//!
//! # JS string semantics
//!
//! `generateHtml` assembles the page with `String.replace(needle, …)`, which
//! replaces the first occurrence only and expands `$` patterns (`$&`, `` $` ``,
//! `$'`, `$$`) in the replacement. The vendored `highlight.min.js` contains a
//! literal `$&`, so upstream's own export rewrites it to the needle text —
//! [`js_replace_first`] reproduces those semantics exactly.

mod ansi_to_html;
mod tool_renderer;

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value};

pub use ansi_to_html::{ansi_lines_to_html, ansi_to_html, strip_ansi_escapes};
pub use tool_renderer::{RenderedToolResult, ToolHtmlRenderer};

use super::json;
use super::store::SessionManager;
use crate::config::APP_NAME;
use crate::modes::interactive::theme::{get_resolved_theme_colors, get_theme_export_colors};
use crate::tools::path_utils::normalize_path;

const TEMPLATE_HTML: &str = include_str!("../../../export-html/template.html");
const TEMPLATE_CSS: &str = include_str!("../../../export-html/template.css");
const TEMPLATE_JS: &str = include_str!("../../../export-html/template.js");
const MARKED_JS: &str = include_str!("../../../export-html/vendor/marked.min.js");
const MARKED_CJK_FRIENDLY_JS: &str =
    include_str!("../../../export-html/vendor/marked-cjk-friendly.min.js");
const HIGHLIGHT_JS: &str = include_str!("../../../export-html/vendor/highlight.min.js");

/// The export-related errors carry upstream's exact `Error` messages; the
/// interactive `/export` handler prefixes `Failed to export session: `.
type ExportResult = Result<PathBuf, String>;

/// `parseColor` (`index.ts:43-61`): hex (`#RRGGBB`) and `rgb(r,g,b)`.
fn parse_color(color: &str) -> Option<(f64, f64, f64)> {
    if let Some((r, g, b)) = hex_triplet(color) {
        return Some((r, g, b));
    }
    // `^rgb\s*\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\)$`
    let rest = color.strip_prefix("rgb")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('(')?;
    let inner = rest.strip_suffix(')')?;
    let mut channels = inner.split(',');
    let parse_channel = |channel: &str| -> Option<f64> {
        let channel = channel.trim();
        if channel.is_empty() || !channel.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        channel.parse::<f64>().ok()
    };
    let r = parse_channel(channels.next()?)?;
    let g = parse_channel(channels.next()?)?;
    let b = parse_channel(channels.next()?)?;
    if channels.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

/// The `^#([0-9a-fA-F]{2})([0-9a-fA-F]{2})([0-9a-fA-F]{2})$` branch of
/// [`parse_color`].
fn hex_triplet(color: &str) -> Option<(f64, f64, f64)> {
    let rest = color.strip_prefix('#')?;
    if rest.len() != 6 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |slice: &str| u32::from_str_radix(slice, 16).ok().map(f64::from);
    let r = channel(&rest[0..2])?;
    let g = channel(&rest[2..4])?;
    let b = channel(&rest[4..6])?;
    Some((r, g, b))
}

/// `getLuminance` (`index.ts:64-70`): relative luminance, 0-1.
fn get_luminance(r: f64, g: f64, b: f64) -> f64 {
    let to_linear = |c: f64| {
        let s = c / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(r) + 0.7152 * to_linear(g) + 0.0722 * to_linear(b)
}

/// `adjustBrightness` (`index.ts:73-78`): factor > 1 lightens, < 1 darkens.
/// An unparseable color passes through unchanged.
fn adjust_brightness(color: &str, factor: f64) -> String {
    let Some((r, g, b)) = parse_color(color) else {
        return color.to_string();
    };
    let adjust = |c: f64| (c * factor).round().clamp(0.0, 255.0);
    format!("rgb({}, {}, {})", adjust(r), adjust(g), adjust(b))
}

/// `deriveExportColors` (`index.ts:81-106`): page/card/info backgrounds
/// derived from a base color such as `userMessageBg`.
fn derive_export_colors(base_color: &str) -> ExportDerivedColors {
    let Some((r, g, b)) = parse_color(base_color) else {
        return ExportDerivedColors {
            page_bg: "rgb(24, 24, 30)".to_string(),
            card_bg: "rgb(30, 30, 36)".to_string(),
            info_bg: "rgb(60, 55, 40)".to_string(),
        };
    };

    let luminance = get_luminance(r, g, b);
    let is_light = luminance > 0.5;

    if is_light {
        return ExportDerivedColors {
            page_bg: adjust_brightness(base_color, 0.96),
            card_bg: base_color.to_string(),
            info_bg: format!(
                "rgb({}, {}, {})",
                (r + 10.0).min(255.0),
                (g + 5.0).min(255.0),
                (b - 20.0).max(0.0)
            ),
        };
    }
    ExportDerivedColors {
        page_bg: adjust_brightness(base_color, 0.7),
        card_bg: adjust_brightness(base_color, 0.85),
        info_bg: format!(
            "rgb({}, {}, {})",
            (r + 20.0).min(255.0),
            (g + 15.0).min(255.0),
            b
        ),
    }
}

/// The derived export backgrounds (`deriveExportColors`'s return shape).
// The fields carry upstream's own names (`pageBg`/`cardBg`/`infoBg`,
// `index.ts:81-106`), so the shared `bg` postfix stays.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExportDerivedColors {
    page_bg: String,
    card_bg: String,
    info_bg: String,
}

/// `generateThemeVars` (`index.ts:111-128`): CSS custom property
/// declarations from the theme colors.
fn generate_theme_vars(theme_name: Option<&str>) -> Result<String, String> {
    let colors = get_resolved_theme_colors(theme_name)?;
    let mut lines: Vec<String> = colors
        .iter()
        .map(|(key, value)| format!("--{key}: {value};"))
        .collect();

    // Use explicit theme export colors if available, otherwise derive from
    // userMessageBg.
    let theme_export = get_theme_export_colors(theme_name);
    let user_message_bg = colors
        .iter()
        .find(|(key, _)| key == "userMessageBg")
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("#343541");
    let derived_colors = derive_export_colors(user_message_bg);

    lines.push(format!(
        "--exportPageBg: {};",
        theme_export
            .page_bg
            .as_ref()
            .unwrap_or(&derived_colors.page_bg)
    ));
    lines.push(format!(
        "--exportCardBg: {};",
        theme_export
            .card_bg
            .as_ref()
            .unwrap_or(&derived_colors.card_bg)
    ));
    lines.push(format!(
        "--exportInfoBg: {};",
        theme_export
            .info_bg
            .as_ref()
            .unwrap_or(&derived_colors.info_bg)
    ));

    Ok(lines.join("\n      "))
}

/// `RenderedToolHtml` (`index.ts:29-33`): pre-rendered HTML for one tool
/// call and its result, keyed by tool call id in `renderedTools`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RenderedToolHtml {
    call_html: Option<String>,
    result_html_collapsed: Option<String>,
    result_html_expanded: Option<String>,
}

/// `ToolDefinition`'s export projection (`index.ts:135`): name, description,
/// and parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// The `AgentState` slice the export consumes (`index.ts:269-270`): the
/// system prompt and the tool declarations.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportState {
    pub system_prompt: Option<String>,
    pub tools: Option<Vec<ExportTool>>,
}

/// `ExportOptions` (`index.ts:35-40`). No `Debug`: the renderer half is a
/// trait object the upstream interface also never formats.
#[derive(Clone, Default)]
pub struct ExportOptions<'a> {
    pub output_path: Option<&'a str>,
    pub theme_name: Option<&'a str>,
    /// Optional tool renderer for custom tools (`None` in this port until
    /// E2 restores the extension renderer plumbing — see [`tool_renderer`]).
    pub tool_renderer: Option<&'a dyn ToolHtmlRenderer>,
}

/// The JSON-serialized session payload (`SessionData`, `index.ts:130-138`).
/// Fields upstream leaves `undefined` are omitted here — that is exactly
/// what `JSON.stringify` does with them.
fn session_data_value(
    header: Option<Map<String, Value>>,
    entries: Vec<Map<String, Value>>,
    leaf_id: Option<&str>,
    state: Option<&ExportState>,
    rendered_tools: Option<&Map<String, Value>>,
) -> Map<String, Value> {
    let mut session_data = Map::new();
    session_data.insert(
        "header".to_string(),
        header.map_or(Value::Null, Value::Object),
    );
    session_data.insert(
        "entries".to_string(),
        Value::Array(entries.into_iter().map(Value::Object).collect()),
    );
    session_data.insert(
        "leafId".to_string(),
        leaf_id.map_or(Value::Null, |id| Value::String(id.to_string())),
    );
    if let Some(state) = state {
        if let Some(system_prompt) = &state.system_prompt {
            session_data.insert(
                "systemPrompt".to_string(),
                Value::String(system_prompt.clone()),
            );
        }
        if let Some(tools) = &state.tools {
            session_data.insert(
                "tools".to_string(),
                Value::Array(
                    tools
                        .iter()
                        .map(|tool| {
                            let mut object = Map::new();
                            object.insert("name".to_string(), Value::String(tool.name.clone()));
                            object.insert(
                                "description".to_string(),
                                Value::String(tool.description.clone()),
                            );
                            object.insert("parameters".to_string(), tool.parameters.clone());
                            Value::Object(object)
                        })
                        .collect(),
                ),
            );
        }
    }
    if let Some(rendered_tools) = rendered_tools {
        session_data.insert(
            "renderedTools".to_string(),
            Value::Object(rendered_tools.clone()),
        );
    }
    session_data
}

/// `String.replace(needle, replacement)` for a string (non-regex) needle:
/// the first occurrence only, with the `$` replacement patterns expanded
/// (`GetSubstitution`, ECMA-262). A string needle has no capture groups, so
/// `$n` and `$<name>` stay literal; `$$`, `$&`, `` $` `` and `$'` do not.
fn js_replace_first(haystack: &str, needle: &str, replacement: &str) -> String {
    let Some(index) = haystack.find(needle) else {
        return haystack.to_string();
    };
    let (head, tail) = haystack.split_at(index);
    let after = &tail[needle.len()..];
    let mut out = String::with_capacity(haystack.len() + replacement.len());
    out.push_str(head);
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                out.push('$');
                chars.next();
            }
            Some('&') => {
                out.push_str(needle);
                chars.next();
            }
            Some('`') => {
                out.push_str(head);
                chars.next();
            }
            Some('\'') => {
                out.push_str(after);
                chars.next();
            }
            _ => out.push('$'),
        }
    }
    out.push_str(after);
    out
}

/// `generateHtml` (`index.ts:143-177`): the core HTML generation shared by
/// both export functions.
fn generate_html(
    session_data: Map<String, Value>,
    theme_name: Option<&str>,
) -> Result<String, String> {
    // Upstream reads the six template files here; the port carries them
    // embedded (see the module doc).
    let theme_vars = generate_theme_vars(theme_name)?;
    let colors = get_resolved_theme_colors(theme_name)?;
    let theme_export = get_theme_export_colors(theme_name);
    let user_message_bg = colors
        .iter()
        .find(|(key, _)| key == "userMessageBg")
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("#343541");
    let derived_export_colors = derive_export_colors(user_message_bg);
    let body_bg = theme_export
        .page_bg
        .as_ref()
        .unwrap_or(&derived_export_colors.page_bg);
    let container_bg = theme_export
        .card_bg
        .as_ref()
        .unwrap_or(&derived_export_colors.card_bg);
    let info_bg = theme_export
        .info_bg
        .as_ref()
        .unwrap_or(&derived_export_colors.info_bg);

    // Base64 encode session data to avoid escaping issues.
    let session_data_base64 =
        BASE64.encode(json::to_string(&Value::Object(session_data)).as_bytes());

    // Build the CSS with theme variables injected.
    let css = js_replace_first(TEMPLATE_CSS, "{{THEME_VARS}}", &theme_vars);
    let css = js_replace_first(&css, "{{BODY_BG}}", body_bg);
    let css = js_replace_first(&css, "{{CONTAINER_BG}}", container_bg);
    let css = js_replace_first(&css, "{{INFO_BG}}", info_bg);

    let html = js_replace_first(TEMPLATE_HTML, "{{CSS}}", &css);
    let html = js_replace_first(&html, "{{JS}}", TEMPLATE_JS);
    let html = js_replace_first(&html, "{{SESSION_DATA}}", &session_data_base64);
    let html = js_replace_first(&html, "{{MARKED_JS}}", MARKED_JS);
    let html = js_replace_first(&html, "{{MARKED_CJK_FRIENDLY_JS}}", MARKED_CJK_FRIENDLY_JS);
    let html = js_replace_first(&html, "{{HIGHLIGHT_JS}}", HIGHLIGHT_JS);
    Ok(html)
}

/// Tools rendered directly by the HTML template, not pre-rendered through
/// the TUI-to-ANSI-to-HTML pipeline (`TEMPLATE_RENDERED_TOOLS`,
/// `index.ts:180`).
const TEMPLATE_RENDERED_TOOLS: [&str; 5] = ["bash", "read", "write", "edit", "ls"];

/// `preRenderCustomTools` (`index.ts:185-232`): render custom tool calls and
/// results to HTML through the renderer. The returned map is keyed by tool
/// call id in entry order.
fn pre_render_custom_tools(
    entries: &[crate::session::SessionEntry],
    tool_renderer: &dyn ToolHtmlRenderer,
) -> Map<String, Value> {
    // The working structure is an insertion-ordered pair list: serde_json's
    // Map API is specialized to `Value`, and a plain HashMap would lose the
    // JS-object insertion order the export embeds.
    let mut rendered_tools: Vec<(String, RenderedToolHtml)> = Vec::new();

    for entry in entries {
        let crate::session::SessionEntry::Message(entry) = entry else {
            continue;
        };
        let crate::session::SessionMessageEntry { message, .. } = entry;

        // Find tool calls in assistant messages
        if let pi_agent::AgentMessage::Assistant(assistant) = message {
            for block in &assistant.content {
                if let pi_ai::types::AssistantContent::ToolCall(call) = block {
                    if TEMPLATE_RENDERED_TOOLS.contains(&call.name.as_str()) {
                        continue;
                    }
                    if let Some(call_html) = tool_renderer.render_call(
                        &call.id,
                        &call.name,
                        &Value::Object(call.arguments.clone()),
                    ) {
                        // `renderedTools[block.id] = { callHtml }` — an
                        // existing entry keeps its position, like a JS
                        // object literal overwrite.
                        match rendered_tools.iter_mut().find(|(id, _)| *id == call.id) {
                            Some((_, slot)) => slot.call_html = Some(call_html),
                            None => rendered_tools.push((
                                call.id.clone(),
                                RenderedToolHtml {
                                    call_html: Some(call_html),
                                    ..RenderedToolHtml::default()
                                },
                            )),
                        }
                    }
                }
            }
        }

        // Find tool results
        if let pi_agent::AgentMessage::ToolResult(result) = message {
            let tool_name = result.tool_name.as_str();
            // Only render if we have a pre-rendered call OR it's not
            // template-rendered.
            let existing = rendered_tools
                .iter()
                .find(|(id, _)| *id == result.tool_call_id);
            if (existing.is_some() || !TEMPLATE_RENDERED_TOOLS.contains(&tool_name))
                && let Some(rendered) = tool_renderer.render_result(
                    &result.tool_call_id,
                    tool_name,
                    &result.content,
                    result.details.as_ref(),
                    result.is_error,
                )
            {
                let mut merged = existing
                    .map_or_else(RenderedToolHtml::default, |(_, rendered)| rendered.clone());
                merged.result_html_collapsed = rendered.collapsed;
                merged.result_html_expanded = rendered.expanded;
                match rendered_tools.iter_mut().find(|(id, _)| *id == result.tool_call_id) {
                    Some((_, slot)) => *slot = merged,
                    None => rendered_tools.push((result.tool_call_id.clone(), merged)),
                }
            }
        }
    }

    rendered_tools
        .into_iter()
        .map(|(id, rendered)| {
            let mut object = Map::new();
            if let Some(call_html) = rendered.call_html {
                object.insert("callHtml".to_string(), Value::String(call_html));
            }
            if let Some(collapsed) = rendered.result_html_collapsed {
                object.insert("resultHtmlCollapsed".to_string(), Value::String(collapsed));
            }
            if let Some(expanded) = rendered.result_html_expanded {
                object.insert("resultHtmlExpanded".to_string(), Value::String(expanded));
            }
            (id, Value::Object(object))
        })
        .collect()
}

/// `index.ts:276-280` (and `:310-314` for files): an explicit output path is
/// normalized (`normalizePath`); the default is `{APP_NAME}-session-<stem>`
/// in the working directory.
fn resolve_export_output_path(output_path: Option<&str>, default: PathBuf) -> PathBuf {
    output_path.map_or(default, normalize_path)
}

/// The default output for a session file: `{APP_NAME}-session-<stem>.html`.
/// `basename(file, ".jsonl")` is the stem (`index.ts:278-280`).
fn default_export_output_name(session_file: &Path) -> PathBuf {
    let stem = session_file
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    PathBuf::from(format!("{APP_NAME}-session-{stem}.html"))
}

/// Write the generated page and return the output path.
fn write_export(html: String, output_path: &Path) -> ExportResult {
    std::fs::write(output_path, html).map_err(|error| error.to_string())?;
    Ok(output_path.to_path_buf())
}

/// `exportSessionToHtml` (`index.ts:238-284`): export a live session to
/// HTML. Used by the interactive `/export` command.
pub fn export_session_to_html(
    session_manager: &SessionManager,
    state: Option<&ExportState>,
    options: &ExportOptions<'_>,
) -> ExportResult {
    let Some(session_file) = session_manager.session_file() else {
        return Err("Cannot export in-memory session to HTML".to_string());
    };
    if !session_file.exists() {
        return Err("Nothing to export yet - start a conversation first".to_string());
    }

    let entries = session_manager.get_entries();

    // Pre-render custom tools if a tool renderer is provided
    let mut rendered_tools: Option<Map<String, Value>> = None;
    if let Some(tool_renderer) = options.tool_renderer {
        let rendered = pre_render_custom_tools(&entries, tool_renderer);
        // Only include if we actually rendered something
        if !rendered.is_empty() {
            rendered_tools = Some(rendered);
        }
    }

    let session_data = session_data_value(
        session_manager
            .get_header_record()
            .map(super::records::Record::into_object),
        session_manager
            .get_entry_records()
            .into_iter()
            .map(super::records::Record::into_object)
            .collect(),
        session_manager.get_leaf_id(),
        state,
        rendered_tools.as_ref(),
    );

    let html = generate_html(session_data, options.theme_name)?;

    let default = default_export_output_name(session_file);
    write_export(
        html,
        &resolve_export_output_path(options.output_path, default),
    )
}

/// `exportFromFile` (`index.ts:290-318`): export a session file to HTML
/// standalone, without agent state. Used by the `--export` CLI flag
/// (`main.ts:688-703`).
pub fn export_from_file(input_path: &str, options: &ExportOptions<'_>) -> ExportResult {
    let resolved_input_path = crate::tools::path_utils::resolve_path(
        input_path,
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    );

    if !resolved_input_path.exists() {
        return Err(format!("File not found: {}", resolved_input_path.display()));
    }

    let session_manager = SessionManager::open(&resolved_input_path, None, None)
        .map_err(|error| error.to_string())?;

    let session_data = session_data_value(
        session_manager
            .get_header_record()
            .map(super::records::Record::into_object),
        session_manager
            .get_entry_records()
            .into_iter()
            .map(super::records::Record::into_object)
            .collect(),
        session_manager.get_leaf_id(),
        None,
        None,
    );

    let html = generate_html(session_data, options.theme_name)?;

    let default = default_export_output_name(&resolved_input_path);
    write_export(
        html,
        &resolve_export_output_path(options.output_path, default),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::NewSessionOptions;
    use std::sync::Mutex;

    /// The template's session-data script tag (`template.html`), which the
    /// export tests decode the base64 payload out of.
    const SESSION_DATA_MARKER: &str = "<script id=\"session-data\" type=\"application/json\">";

    /// A renderer double: renders one call and one result, like a real
    /// extension tool would.
    struct FakeRenderer {
        calls: Mutex<Vec<(String, String)>>,
        results: Mutex<Vec<(String, String)>>,
    }

    impl FakeRenderer {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                results: Mutex::new(Vec::new()),
            }
        }
    }

    impl ToolHtmlRenderer for FakeRenderer {
        fn render_call(
            &self,
            tool_call_id: &str,
            tool_name: &str,
            _args: &Value,
        ) -> Option<String> {
            self.calls
                .lock()
                .expect("calls")
                .push((tool_call_id.to_string(), tool_name.to_string()));
            Some(format!("<div>call {tool_call_id} {tool_name}</div>"))
        }

        fn render_result(
            &self,
            tool_call_id: &str,
            tool_name: &str,
            _result: &[pi_ai::types::InputContent],
            _details: Option<&Value>,
            _is_error: bool,
        ) -> Option<RenderedToolResult> {
            self.results
                .lock()
                .expect("results")
                .push((tool_call_id.to_string(), tool_name.to_string()));
            Some(RenderedToolResult {
                collapsed: None,
                expanded: Some(format!("<div>result {tool_call_id} {tool_name}</div>")),
            })
        }
    }

    #[test]
    fn parse_color_accepts_hex_and_rgb_forms() {
        assert_eq!(parse_color("#343541"), Some((52.0, 53.0, 65.0)));
        assert_eq!(parse_color("rgb(24, 24, 30)"), Some((24.0, 24.0, 30.0)));
        assert_eq!(parse_color("rgb( 1 , 2 , 3 )"), Some((1.0, 2.0, 3.0)));
        assert_eq!(parse_color("#345"), None);
        assert_eq!(parse_color("rgb(1,2)"), None);
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn luminance_splits_light_from_dark() {
        // White is light, near-black is dark.
        assert!(get_luminance(255.0, 255.0, 255.0) > 0.5);
        assert!(get_luminance(10.0, 10.0, 10.0) < 0.5);
    }

    #[test]
    fn adjust_brightness_clamps_and_rounds() {
        assert_eq!(adjust_brightness("#ffffff", 0.96), "rgb(245, 245, 245)");
        assert_eq!(adjust_brightness("#343541", 0.7), "rgb(36, 37, 46)");
        assert_eq!(adjust_brightness("not-a-color", 2.0), "not-a-color");
        // Clamp at 255.
        assert_eq!(adjust_brightness("#ffffff", 10.0), "rgb(255, 255, 255)");
    }

    #[test]
    fn derive_export_colors_has_the_literal_fallbacks() {
        let derived = derive_export_colors("nope");
        assert_eq!(derived.page_bg, "rgb(24, 24, 30)");
        assert_eq!(derived.card_bg, "rgb(30, 30, 36)");
        assert_eq!(derived.info_bg, "rgb(60, 55, 40)");
    }

    #[test]
    fn derive_export_colors_dark_branch() {
        // userMessageBg's default (#343541) is dark: brightened variants.
        let derived = derive_export_colors("#343541");
        assert_eq!(derived.page_bg, "rgb(36, 37, 46)");
        assert_eq!(derived.card_bg, "rgb(44, 45, 55)");
        assert_eq!(derived.info_bg, "rgb(72, 68, 65)");
    }

    #[test]
    fn derive_export_colors_light_branch() {
        let derived = derive_export_colors("#ececf1");
        assert_eq!(derived.page_bg, adjust_brightness("#ececf1", 0.96));
        assert_eq!(derived.card_bg, "#ececf1");
        // rgb(min(255, 236+10), min(255, 236+5), max(0, 241-20)).
        assert_eq!(derived.info_bg, "rgb(246, 241, 221)");
    }

    /// The theme vars carry every token plus the three export variables, in
    /// `--key: value;` lines joined with the six-space separator.
    #[test]
    fn theme_vars_list_tokens_and_export_variables() {
        let vars = generate_theme_vars(Some("dark")).expect("dark loads");
        assert!(vars.starts_with("--accent: "), "{vars}");
        assert!(vars.contains("\n      --"), "{vars}");
        for export_var in ["--exportPageBg: ", "--exportCardBg: ", "--exportInfoBg: "] {
            assert!(vars.contains(export_var), "{vars}");
        }
        // The dark theme document declares explicit export colors.
        assert!(vars.contains("--exportPageBg: #"), "{vars}");
    }

    /// `export-html-whitespace.test.ts:5-11`: the template CSS carries the
    /// whitespace rules the export relies on (pinned by searching the
    /// embedded copy).
    #[test]
    fn template_css_keeps_the_whitespace_rules() {
        assert!(
            TEMPLATE_CSS.contains(".output-preview > div:not(.expand-hint)")
                && TEMPLATE_CSS.contains(".output-full > div:not(.expand-hint)"),
            "output containers present"
        );
        assert!(TEMPLATE_CSS.contains("white-space: pre-wrap;"));
        assert!(TEMPLATE_CSS.contains(".ansi-line"));
        assert!(TEMPLATE_CSS.contains("white-space: pre;"));
    }

    /// The vendored templates match the upstream files byte for byte — the
    /// export ships them unchanged.
    #[test]
    fn embedded_templates_are_upstream_bytes() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let upstream_dir =
            std::path::PathBuf::from("/tmp/pi/packages/coding-agent/src/core/export-html");
        // The upstream clone is the ground truth when present; the committed
        // copies under export-html/ are the shipped bytes either way.
        if !upstream_dir.exists() {
            return;
        }
        for relative in [
            "template.html",
            "template.css",
            "template.js",
            "vendor/marked.min.js",
            "vendor/marked-cjk-friendly.min.js",
            "vendor/highlight.min.js",
        ] {
            let committed = std::fs::read(manifest.join("export-html").join(relative))
                .expect("committed template");
            let upstream = std::fs::read(upstream_dir.join(relative)).expect("upstream template");
            assert_eq!(committed, upstream, "{relative} diverged from upstream");
        }
        // And the embedded constants are those committed bytes.
        let committed_html =
            std::fs::read(manifest.join("export-html/template.html")).expect("html");
        assert_eq!(TEMPLATE_HTML.as_bytes(), committed_html.as_slice());
        let committed_hljs =
            std::fs::read(manifest.join("export-html/vendor/highlight.min.js")).expect("hljs");
        assert_eq!(HIGHLIGHT_JS.as_bytes(), committed_hljs.as_slice());
    }

    /// `js_replace_first` reproduces `String.replace`'s first-occurrence and
    /// `$`-pattern semantics, including the `$&` that lives inside
    /// `highlight.min.js`.
    #[test]
    fn js_replace_first_expands_replacement_patterns() {
        assert_eq!(js_replace_first("aXbXc", "X", "Y"), "aYbXc");
        assert_eq!(js_replace_first("abc", "zz", "Y"), "abc");
        assert_eq!(js_replace_first("abc", "b", "$&$&"), "abbc");
        // "$`" is the text before the match: "a" + replacement "a" + "c".
        assert_eq!(js_replace_first("abc", "b", "$`"), "aac");
        // "$'" is the text after the match: "a" + replacement "c" + "c".
        assert_eq!(js_replace_first("abc", "b", "$'"), "acc");
        // "$$" is one literal dollar: "a" + "$" + "c".
        assert_eq!(js_replace_first("abc", "b", "$$"), "a$c");
        // "$n" with n > captures (0 for a string pattern) stays literal:
        // "a" + "$1" + "c".
        assert_eq!(js_replace_first("abc", "b", "$1"), "a$1c");
        assert_eq!(js_replace_first("[-+*?!$&|]", "$&", "X"), "[-+*?!X|]");
    }

    #[test]
    fn generate_html_embeds_all_pieces() {
        let mut session_data = Map::new();
        session_data.insert("header".to_string(), Value::Null);
        session_data.insert("entries".to_string(), Value::Array(vec![]));
        session_data.insert("leafId".to_string(), Value::Null);
        let html = generate_html(session_data, Some("dark")).expect("generate");
        // The needles are gone — except `{{HIGHLIGHT_JS}}`: the template's
        // needle is the match itself, and the vendored highlight.min.js
        // carries a literal `$&`, which `String.replace` expands back to the
        // needle text — so exactly one occurrence remains (the module doc's
        // quirk; upstream's replace behaves identically).
        for needle in [
            "{{THEME_VARS}}",
            "{{BODY_BG}}",
            "{{CONTAINER_BG}}",
            "{{INFO_BG}}",
            "{{CSS}}",
            "{{JS}}",
            "{{SESSION_DATA}}",
            "{{MARKED_JS}}",
            "{{MARKED_CJK_FRIENDLY_JS}}",
        ] {
            assert!(!html.contains(needle), "leftover {needle}");
        }
        assert_eq!(
            html.matches("{{HIGHLIGHT_JS}}").count(),
            1,
            "the template's needle is the match; the `$&` rewrite re-inserts it"
        );
        // The session data rides base64-encoded inside the JSON script tag
        // (`template.html`).
        let payload_start = html
            .find(SESSION_DATA_MARKER)
            .map(|index| index + SESSION_DATA_MARKER.len())
            .expect("session data script tag");
        let payload = &html[payload_start..html.find("</script>").expect("script close")];
        let decoded = String::from_utf8(BASE64.decode(payload).expect("base64")).expect("utf8");
        assert_eq!(decoded, "{\"header\":null,\"entries\":[],\"leafId\":null}");
    }

    fn create_session(dir: &Path) -> crate::session::SessionManager {
        crate::session::SessionManager::create(dir, Some(dir), &NewSessionOptions::default())
            .expect("session")
    }

    /// A conversation-starting message (the store's first flush creates the
    /// session file — see `lazy_first_flush_needs_a_conversation`).
    fn start_conversation(session: &mut crate::session::SessionManager, text: &str) {
        session
            .append_message(pi_agent::transcript::user_message(text, 1))
            .expect("append");
    }

    fn assistant_with_tool_calls(calls: &[(&str, &str)]) -> pi_agent::AgentMessage {
        use pi_ai::types::{AssistantContent, AssistantMessage, ToolCall, Usage, UsageCost};
        pi_agent::AgentMessage::Assistant(AssistantMessage {
            content: calls
                .iter()
                .map(|(id, name)| {
                    AssistantContent::ToolCall(ToolCall {
                        id: (*id).to_string(),
                        name: (*name).to_string(),
                        arguments: Map::new(),
                        thought_signature: None,
                        namespace: None,
                        arguments_error: None,
                    })
                })
                .collect(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "claude-test".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: Vec::new(),
            usage: Usage {
                input: 1,
                output: 1,
                cache_read: 0,
                cache_write: 0,
                cache_write_1h: None,
                reasoning: None,
                total_tokens: 2,
                cost: UsageCost::default(),
            },
            stop_reason: pi_ai::types::StopReason::ToolUse,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
            duration_ms: None,
        })
    }

    /// `exportSessionToHtml`'s guards: no session file yet reports the exact
    /// upstream message, and an in-memory session cannot export at all.
    #[test]
    fn export_session_to_html_guards() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = create_session(dir.path());
        // Setup entries never touch disk; the export refuses until a
        // conversation starts.
        assert_eq!(
            export_session_to_html(&session, None, &ExportOptions::default()),
            Err("Nothing to export yet - start a conversation first".to_string())
        );
        let in_memory = crate::session::SessionManager::in_memory(
            dir.path(),
            &NewSessionOptions::default(),
            Vec::new(),
        )
        .expect("in-memory");
        assert_eq!(
            export_session_to_html(&in_memory, None, &ExportOptions::default()),
            Err("Cannot export in-memory session to HTML".to_string())
        );
    }

    #[test]
    fn export_session_to_html_writes_the_requested_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = create_session(dir.path());
        start_conversation(&mut session, "hello");
        let out = dir.path().join("out.html");
        let path = export_session_to_html(
            &session,
            None,
            &ExportOptions {
                output_path: out.to_str(),
                ..ExportOptions::default()
            },
        )
        .expect("export");
        assert_eq!(path, out);
        let html = std::fs::read_to_string(&path).expect("html");
        assert!(html.starts_with("<!DOCTYPE html>"), "{html}");
    }

    /// `index.ts:278-280`: without an output path the file is
    /// `{APP_NAME}-session-{basename}.html` in the working directory.
    #[test]
    fn default_export_output_name_stems_the_session_file() {
        let source = Path::new("/sessions/2026-01-01T00_00_00_000Z_abc.jsonl");
        let name = default_export_output_name(source);
        assert_eq!(
            name,
            Path::new(&format!(
                "{APP_NAME}-session-2026-01-01T00_00_00_000Z_abc.html"
            ))
        );
    }

    #[test]
    fn export_from_file_reports_missing_files_like_upstream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.jsonl");
        let error = export_from_file(missing.to_str().expect("utf8"), &ExportOptions::default())
            .expect_err("missing input");
        assert_eq!(error, format!("File not found: {}", missing.display()));
    }

    #[test]
    fn export_from_file_exports_a_session_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = create_session(dir.path());
        start_conversation(&mut session, "hello");
        let source = dir.path().join("source.jsonl");
        std::fs::copy(session.session_file().expect("file"), &source).expect("copy");
        let out = dir.path().join("out.html");
        let path = export_from_file(
            source.to_str().expect("utf8"),
            &ExportOptions {
                output_path: out.to_str(),
                ..ExportOptions::default()
            },
        )
        .expect("export");
        assert_eq!(path, out);
        let html = std::fs::read_to_string(&path).expect("html");
        // The payload rides base64-encoded in the session-data script tag;
        // decode it to find the message.
        let payload_start = html
            .find(SESSION_DATA_MARKER)
            .map(|index| index + SESSION_DATA_MARKER.len())
            .expect("session data script tag");
        let payload = &html[payload_start..html.find("</script>").expect("script close")];
        let decoded = String::from_utf8(BASE64.decode(payload).expect("base64")).expect("utf8");
        assert!(decoded.contains("hello"), "{decoded}");
    }

    /// `preRenderCustomTools` skips template-rendered tools and merges call
    /// and result HTML per tool call id.
    #[test]
    fn pre_render_custom_tools_renders_only_custom_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = create_session(dir.path());
        start_conversation(&mut session, "run");
        let _ = session.append_message(assistant_with_tool_calls(&[
            ("call-1", "bash"),
            ("call-2", "custom_tool"),
        ]));
        let tool_result = pi_agent::AgentMessage::ToolResult(pi_ai::types::ToolResultMessage {
            tool_call_id: "call-2".to_string(),
            tool_name: "custom_tool".to_string(),
            content: vec![pi_ai::types::InputContent::Text(
                pi_ai::types::TextContent {
                    text: "done".to_string(),
                    text_signature: None,
                },
            )],
            details: None,
            usage: None,
            nested_calls: None,
            is_error: false,
            timestamp: 1,
            duration_ms: None,
        });
        let _ = session.append_message(tool_result);

        let renderer = FakeRenderer::new();
        let rendered_map = pre_render_custom_tools(&session.get_entries(), &renderer);
        // bash (template-rendered) was never rendered; the custom tool's
        // call and result merged under one id.
        assert_eq!(rendered_map.len(), 1, "{rendered_map:?}");
        let entry = rendered_map.get("call-2").expect("call-2 rendered");
        assert_eq!(
            entry.get("callHtml").and_then(Value::as_str),
            Some("<div>call call-2 custom_tool</div>")
        );
        assert_eq!(
            entry.get("resultHtmlExpanded").and_then(Value::as_str),
            Some("<div>result call-2 custom_tool</div>")
        );
        assert!(entry.get("resultHtmlCollapsed").is_none());
        assert_eq!(renderer.calls.lock().expect("calls").len(), 1);
        assert_eq!(renderer.results.lock().expect("results").len(), 1);
    }

    /// With a renderer present the export embeds the rendered tools in the
    /// base64 session payload; without one the key is absent, exactly like
    /// `JSON.stringify` dropping the `undefined`.
    #[test]
    fn export_session_to_html_embeds_rendered_tools_only_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = create_session(dir.path());
        start_conversation(&mut session, "x");
        let _ = session.append_message(assistant_with_tool_calls(&[("call-9", "custom_tool")]));

        // Decode the session-data script tag's payload.
        let payload_of = |path: &Path| -> String {
            let html = std::fs::read_to_string(path).expect("html");
            let start = html
                .find(SESSION_DATA_MARKER)
                .map(|index| index + SESSION_DATA_MARKER.len())
                .expect("tag");
            let payload = &html[start..html.find("</script>").expect("close")];
            String::from_utf8(BASE64.decode(payload).expect("base64")).expect("utf8")
        };

        let out_a = dir.path().join("with-renderer.html");
        let renderer = FakeRenderer::new();
        export_session_to_html(
            &session,
            None,
            &ExportOptions {
                output_path: out_a.to_str(),
                tool_renderer: Some(&renderer),
                ..ExportOptions::default()
            },
        )
        .expect("export with renderer");
        let payload_a = payload_of(&out_a);
        assert!(payload_a.contains("call call-9 custom_tool"), "{payload_a}");
        assert!(payload_a.contains("\"renderedTools\""), "{payload_a}");

        let out_b = dir.path().join("without-renderer.html");
        export_session_to_html(
            &session,
            None,
            &ExportOptions {
                output_path: out_b.to_str(),
                ..ExportOptions::default()
            },
        )
        .expect("export without renderer");
        let payload_b = payload_of(&out_b);
        assert!(
            !payload_b.contains("call call-9 custom_tool"),
            "{payload_b}"
        );
        assert!(!payload_b.contains("renderedTools"), "{payload_b}");
    }
}
