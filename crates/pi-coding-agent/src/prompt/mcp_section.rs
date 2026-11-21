//! The `mcp_servers` system-prompt section.
//!
//! Ground truth: pi-mono `packages/coding-agent/src/extensions/mcp/index.ts`
//! (`MCP_SERVERS_SECTION` and `MAX_SERVER_DESCRIPTION_CHARS` at lines 157-159,
//! `MAX_SERVERS_SECTION_CHARS` at 164, `serversSectionIntro` at 166-172,
//! `truncate` at 174-177, `McpServerListing`/`serverSummary` at 179-189, and
//! `renderServersSection` at 191-222) plus `mcpNamespace` in
//! `packages/mcp/src/naming.ts:33-35`. The host invokes the renderer from its
//! `before_agent_start` handler (`index.ts:1177-1185`) and inserts the text as
//! the `mcp_servers` section of `event.systemPromptOptions.sections`.
//!
//! This module is deliberately dependency-free: callers map whatever MCP
//! config/connection snapshot they have into [`McpServerListing`] values. It
//! does not import the config loader or the `pi mcp` CLI slice.

use std::cmp::Ordering;

/// Name of the system-prompt section (`index.ts:157`).
pub const MCP_SERVERS_SECTION: &str = "mcp_servers";

/// Characters of a single server description (`index.ts:159`). Codex's limit
/// for deferred namespaces.
const MAX_SERVER_DESCRIPTION_CHARS: usize = 250;

/// Characters of the whole section (`index.ts:164`).
const MAX_SERVERS_SECTION_CHARS: usize = 4096;

/// How a server's tools are reached (`core/mcp-servers.ts:61`). The section
/// lists only `Codemode` and `Deferred` servers; `Direct` tools are declared
/// to the model and `Hidden` tools are unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpExposure {
    /// Callable from codemode scripts, which find them with `searchTools()`.
    Codemode,
    /// Not declared until `tool_search` loads them, then called directly.
    Deferred,
    /// Declared to the model like built-in tools.
    Direct,
    /// Registered but unreachable.
    Hidden,
}

impl McpExposure {
    /// Whether the exposure is reached indirectly (`index.ts:150-154`).
    fn is_indirect(self) -> bool {
        matches!(self, Self::Codemode | Self::Deferred)
    }
}

/// What the `mcp_servers` section needs of one server (`index.ts:180-183`).
///
/// A flattened, independent snapshot: the host maps its own MCP config entry
/// and connection state into these fields before rendering. `tool_exposure`
/// holds only the *values* of `config.toolExposure` (`mcp-servers.ts:86`),
/// because `configuredExposures` (`index.ts:141-143`) ignores the keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerListing {
    /// `entry.name` (`extensions/mcp/config.ts:49`).
    pub name: String,
    /// `entry.config.enabled !== false` (`index.ts:132-134`); defaults to true.
    pub enabled: bool,
    /// `entry.config.exposure ?? "codemode"` (`index.ts:136-138`).
    pub exposure: McpExposure,
    /// The values of `entry.config.toolExposure`, in insertion order.
    pub tool_exposure: Vec<McpExposure>,
    /// `entry.config.description` (`index.ts:186-189`).
    pub description: Option<String>,
    /// `connection.instructions` (`index.ts:182`), the fallback for the summary.
    pub instructions: Option<String>,
}

impl McpServerListing {
    /// A listing with upstream's defaults: enabled, codemode, no overrides.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            enabled: true,
            exposure: McpExposure::Codemode,
            tool_exposure: Vec::new(),
            description: None,
            instructions: None,
        }
    }

    /// `configuredExposures` (`index.ts:141-143`): the server default plus the
    /// per-tool overrides.
    fn configured_exposures(&self) -> impl Iterator<Item = McpExposure> + '_ {
        std::iter::once(self.exposure).chain(self.tool_exposure.iter().copied())
    }

    /// `hasIndirectTools` (`index.ts:150-154`) for this server.
    fn has_indirect_tools(&self) -> bool {
        self.configured_exposures().any(McpExposure::is_indirect)
    }

    /// `index.ts:201-203`: codemode wins; otherwise the server is reached with
    /// `tool_search`.
    fn reach(&self) -> &'static str {
        if self
            .configured_exposures()
            .any(|exposure| exposure == McpExposure::Codemode)
        {
            "codemode"
        } else {
            "tool_search"
        }
    }

    /// `serverSummary` (`index.ts:185-189`): first line of the trimmed
    /// configured description, else of the connection instructions.
    fn summary(&self) -> String {
        let text = self
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .or(self.instructions.as_deref())
            .unwrap_or("");
        text.split('\n').next().unwrap_or("").trim().to_string()
    }
}

/// `mcpNamespace` (`packages/mcp/src/naming.ts:33-35`): `mcp__<server>` with
/// `-` replaced by `_`.
fn mcp_namespace(name: &str) -> String {
    format!("mcp__{}", name.replace('-', "_"))
}

/// JS `String.length` in UTF-16 code units, used by `size` and `truncate`
/// (`index.ts:174-177, 209, 216`).
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// pi's `truncate` (`index.ts:174-177`). `sliceHead` takes at most `max - 1`
/// UTF-16 code units without splitting a surrogate pair; the tail is trimmed
/// and an ellipsis appended. Iterating whole chars gives the same boundary.
fn truncate(text: &str, max: usize) -> String {
    if utf16_len(text) <= max {
        return text.to_string();
    }
    if max <= 1 {
        return String::new();
    }
    let mut head = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let width = character.len_utf16();
        if used + width > max - 1 {
            break;
        }
        head.push(character);
        used += width;
    }
    format!("{}…", head.trim_end())
}

/// `serversSectionIntro` (`index.ts:166-172`): explain only the ways of
/// reaching tools that the listed servers actually use.
fn servers_section_intro(reaches: &[&'static str]) -> String {
    let mut intro = "MCP servers whose tools are not declared to you.".to_string();
    if reaches.contains(&"codemode") {
        intro.push_str(" Call the tools of `codemode` servers from codemode scripts.");
    }
    if reaches.contains(&"tool_search") {
        intro.push_str(" Load the tools of `tool_search` servers with `tool_search`.");
    }
    intro
}

/// `omitted` (`index.ts:206-207`).
fn omitted_line(count: usize) -> Option<String> {
    (count > 0).then(|| {
        format!(
            "- … {count} more server{}; find their tools with searchTools()",
            if count == 1 { "" } else { "s" }
        )
    })
}

/// Approximate JS `String.prototype.localeCompare` (the default en-US
/// collation used by `index.ts:199`) without an ICU dependency: compare
/// case-insensitively, then put lowercase before uppercase, then by code
/// point. Server names are short identifiers, so this preserves upstream's
/// ordering for them.
fn locale_compare(left: &str, right: &str) -> Ordering {
    let left_lower = left.to_lowercase();
    let right_lower = right.to_lowercase();
    left_lower.cmp(&right_lower).then_with(|| {
        left.chars()
            .map(case_rank)
            .cmp(right.chars().map(case_rank))
    })
}

/// Case tie-break for `locale_compare`: lowercase (and uncased) first.
fn case_rank(character: char) -> u8 {
    u8::from(character.is_uppercase())
}

/// pi's `renderServersSection` (`index.ts:191-222`).
///
/// Every enabled server with codemode or deferred tools, sorted by name, with
/// how its tools are reached and a one-line summary. `None` when there are no
/// such servers, in which case the host must omit the section. Overlong
/// descriptions shrink first; if the server lines alone exceed the budget the
/// last servers are dropped and counted in a closing line.
pub fn render_servers_section(servers: &[McpServerListing]) -> Option<String> {
    let mut listed: Vec<&McpServerListing> = servers
        .iter()
        .filter(|server| server.enabled && server.has_indirect_tools())
        .collect();
    listed.sort_by(|left, right| locale_compare(&left.name, &right.name));
    if listed.is_empty() {
        return None;
    }

    let reaches: Vec<&'static str> = listed.iter().map(|server| server.reach()).collect();
    let intro = servers_section_intro(&reaches);
    let heads: Vec<String> = listed
        .iter()
        .zip(&reaches)
        .map(|(server, reach)| format!("- {} ({reach})", mcp_namespace(&server.name)))
        .collect();

    // `size` (`index.ts:208-211`): the intro, the first `kept` heads and the
    // omission line, joined with newlines.
    let size = |kept: usize| -> usize {
        let omitted = omitted_line(listed.len() - kept);
        let mut length = utf16_len(&intro);
        for head in heads.iter().take(kept) {
            length += 1 + utf16_len(head);
        }
        if let Some(line) = omitted {
            length += 1 + utf16_len(&line);
        }
        length
    };

    let mut kept = listed.len();
    while kept > 0 && size(kept) > MAX_SERVERS_SECTION_CHARS {
        kept -= 1;
    }
    // `perServer` (`index.ts:213-216`): each description also takes a ": ".
    let per_server = if kept == 0 {
        0i64
    } else {
        let budget = (MAX_SERVERS_SECTION_CHARS as i64 - size(kept) as i64) / kept as i64 - 2;
        budget.min(MAX_SERVER_DESCRIPTION_CHARS as i64)
    };

    let mut lines = Vec::with_capacity(kept + 2);
    lines.push(intro);
    for (index, server) in listed.iter().take(kept).enumerate() {
        let summary = if per_server > 0 {
            truncate(&server.summary(), per_server as usize)
        } else {
            String::new()
        };
        if summary.is_empty() {
            lines.push(heads[index].clone());
        } else {
            lines.push(format!("{}: {summary}", heads[index]));
        }
    }
    if let Some(line) = omitted_line(listed.len() - kept) {
        lines.push(line);
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SERVERS_SECTION_CHARS, McpExposure, McpServerListing, mcp_namespace, omitted_line,
        render_servers_section, truncate, utf16_len,
    };

    fn server(name: &str) -> McpServerListing {
        McpServerListing::new(name)
    }

    #[test]
    fn empty_snapshot_is_omitted() {
        assert_eq!(render_servers_section(&[]), None);
    }

    #[test]
    fn declared_disabled_and_hidden_servers_are_omitted() {
        let mut direct = server("docs");
        direct.exposure = McpExposure::Direct;
        let mut disabled = server("secret");
        disabled.enabled = false;
        let mut hidden = server("hidden");
        hidden.exposure = McpExposure::Hidden;
        assert_eq!(render_servers_section(&[direct, disabled, hidden]), None);
    }

    #[test]
    fn renders_one_codemode_server_with_description() {
        let mut docs = server("docs");
        docs.description = Some("Docs search.\nMore guidance.".to_string());
        assert_eq!(
            render_servers_section(&[docs]).as_deref(),
            Some(
                "MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts.\n\
                 - mcp__docs (codemode): Docs search."
            )
        );
    }

    #[test]
    fn renders_one_deferred_server_with_instructions_fallback() {
        let mut docs = server("docs");
        docs.exposure = McpExposure::Deferred;
        docs.instructions = Some("Docs search.\nLong guidance.".to_string());
        let section = render_servers_section(&[docs]).unwrap();
        assert_eq!(
            section,
            "MCP servers whose tools are not declared to you. Load the tools of `tool_search` servers with `tool_search`.\n\
             - mcp__docs (tool_search): Docs search."
        );
        assert!(!section.contains("Long guidance"));
    }

    #[test]
    fn prefers_configured_description_over_instructions() {
        let mut docs = server("docs");
        docs.description = Some(" Configured summary. \nignored".to_string());
        docs.instructions = Some("From instructions.".to_string());
        assert_eq!(
            render_servers_section(&[docs]).as_deref(),
            Some(
                "MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts.\n\
                 - mcp__docs (codemode): Configured summary."
            )
        );
    }

    #[test]
    fn falls_back_to_instructions_when_description_is_blank() {
        let mut docs = server("docs");
        docs.description = Some("   ".to_string());
        docs.instructions = Some("From instructions.\nignored".to_string());
        let section = render_servers_section(&[docs]).unwrap();
        assert!(section.ends_with("- mcp__docs (codemode): From instructions."));
    }

    #[test]
    fn sorts_by_name_converts_namespace_and_notes_both_reaches() {
        let mut zeta = server("zeta-server");
        zeta.exposure = McpExposure::Deferred;
        let servers = vec![zeta, server("Alpha"), server("beta")];
        assert_eq!(
            render_servers_section(&servers).as_deref(),
            Some(
                "MCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts. Load the tools of `tool_search` servers with `tool_search`.\n\
                 - mcp__Alpha (codemode)\n\
                 - mcp__beta (codemode)\n\
                 - mcp__zeta_server (tool_search)"
            )
        );
    }

    #[test]
    fn tool_exposure_overrides_decide_listing_and_reach() {
        let mut hidden = server("hidden");
        hidden.exposure = McpExposure::Hidden;
        hidden.tool_exposure = vec![McpExposure::Codemode];
        let mut direct = server("direct");
        direct.exposure = McpExposure::Direct;
        direct.tool_exposure = vec![McpExposure::Deferred];
        let section = render_servers_section(&[hidden, direct]).unwrap();
        assert_eq!(
            section.lines().skip(1).collect::<Vec<_>>(),
            ["- mcp__direct (tool_search)", "- mcp__hidden (codemode)"]
        );
    }

    #[test]
    fn direct_default_with_hidden_overrides_is_omitted() {
        let mut only_direct = server("only-direct");
        only_direct.exposure = McpExposure::Direct;
        only_direct.tool_exposure = vec![McpExposure::Hidden];
        assert_eq!(render_servers_section(&[only_direct]), None);
    }

    #[test]
    fn omission_line_handles_singular_and_plural() {
        assert_eq!(omitted_line(0), None);
        assert_eq!(
            omitted_line(1).as_deref(),
            Some("- … 1 more server; find their tools with searchTools()")
        );
        assert_eq!(
            omitted_line(2).as_deref(),
            Some("- … 2 more servers; find their tools with searchTools()")
        );
    }

    #[test]
    fn shortening_descriptions_keeps_every_server() {
        let servers = (0..40)
            .map(|index| {
                let mut entry = server(&format!("server{index}"));
                entry.description = Some("x".repeat(400));
                entry
            })
            .collect::<Vec<_>>();
        let section = render_servers_section(&servers).unwrap();
        assert!(utf16_len(&section) <= MAX_SERVERS_SECTION_CHARS);
        assert_eq!(section.lines().count(), 41);
        assert!(section.contains("- mcp__server39 (codemode): x"));
    }

    #[test]
    fn omits_trailing_servers_when_names_alone_do_not_fit() {
        let servers = (0..200)
            .map(|index| {
                let mut entry = server(&format!("server-with-a-long-name-{index}"));
                entry.description = Some("desc".to_string());
                entry
            })
            .collect::<Vec<_>>();
        let section = render_servers_section(&servers).unwrap();
        assert!(utf16_len(&section) <= MAX_SERVERS_SECTION_CHARS);
        let lines = section.lines().collect::<Vec<_>>();
        let last = lines.last().copied().unwrap();
        assert!(last.starts_with("- … "));
        assert!(last.ends_with(" more servers; find their tools with searchTools()"));
        let omitted = last
            .split_whitespace()
            .nth(2)
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(lines.len() - 2 + omitted, 200);
    }

    #[test]
    fn truncate_matches_js_code_unit_semantics() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 3), "ab…");
        assert_eq!(truncate("abcdef", 1), "");
        assert_eq!(truncate("abc  ", 4), "abc…");
        assert_eq!(truncate("😀x", 2), "…");
    }

    #[test]
    fn namespace_replaces_dashes() {
        assert_eq!(mcp_namespace("my-server"), "mcp__my_server");
    }
}
