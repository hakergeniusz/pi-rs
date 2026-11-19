//! `FooterComponent` — the two status lines at the bottom of the dock: the
//! working directory (with git branch and session name) and the token/cost/
//! context stats with the model on the right. The right cell adds the
//! thinking level for reasoning models (` • thinking off` or the level), the
//! routed virtual model (` → id[ • level]`), and a `(provider)` prefix when
//! more than one provider has available models and the line still fits.
//!
//! Ground truth: `modes/interactive/components/footer.ts` (the right cell at
//! `footer.ts:219-255`; the provider count from `footer-data-provider.ts` +
//! `interactive-mode.ts:5444-5451`).

use std::path::Path;

use pi_tui::ansi::{truncate_to_width, visible_width};
use pi_tui::theme::ThemeColor;

use super::super::theme;

/// `formatTokens` (`footer.ts:25-31`).
pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", count / 1000);
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", count / 1_000_000)
}

/// `formatCwdForFooter` (`footer.ts:33-45`): `~`-relative when inside home.
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return cwd.to_string();
    };
    let cwd_path = Path::new(cwd);
    let home_path = Path::new(home);
    let Ok(relative) = cwd_path.strip_prefix(home_path) else {
        return cwd.to_string();
    };
    if relative.as_os_str().is_empty() {
        return "~".to_string();
    }
    format!(
        "~{}{}",
        std::path::MAIN_SEPARATOR,
        relative.to_string_lossy()
    )
}

/// `UsageTotals` (`core/usage-totals.ts`): cumulative session usage.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

impl UsageTotals {
    /// `addUsageToTotals` (`core/usage-totals.ts`).
    pub fn add(&mut self, usage: &pi_ai::types::Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.cost += usage.cost.total;
    }

    /// `tokens` (`core/usage-totals.ts:88`): the four token buckets.
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// One `getUsageCostBreakdown` row (`UsageCostBreakdownEntry`,
/// `core/usage-totals.ts:67-72`).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageCostBreakdownEntry {
    /// `provider/model` for model-attributed usage, `Tools/summaries` for
    /// tool-result / branch-summary / compaction usage.
    pub key: String,
    pub cost: f64,
    pub tokens: u64,
}

/// `getUsageCostBreakdown` (`core/usage-totals.ts:78-99`): group
/// model-attributed usage by model and all other usage into a separate
/// bucket, most expensive first. Usage without model attribution is grouped
/// separately so the breakdown reconciles with the session total.
pub fn get_usage_cost_breakdown(
    entries: &[crate::session::records::SessionEntry],
) -> Vec<UsageCostBreakdownEntry> {
    use crate::session::records::SessionEntry;
    use pi_agent::agent_message::AgentMessage;

    let mut totals_by_key: Vec<(String, UsageTotals)> = Vec::new();
    let mut add = |key: String, usage: &pi_ai::types::Usage| {
        if let Some((_, totals)) = totals_by_key.iter_mut().find(|(k, _)| *k == key) {
            totals.add(usage);
        } else {
            let mut totals = UsageTotals::default();
            totals.add(usage);
            totals_by_key.push((key, totals));
        }
    };

    for entry in entries {
        match entry {
            SessionEntry::Message(entry) => match &entry.message {
                AgentMessage::Assistant(message) => {
                    let key = format!(
                        "{}/{}",
                        message.provider,
                        message.response_model.as_deref().unwrap_or(&message.model)
                    );
                    add(key, &message.usage);
                }
                AgentMessage::ToolResult(message) => {
                    if let Some(usage) = &message.usage {
                        add("Tools/summaries".to_string(), usage);
                    }
                }
                _ => {}
            },
            SessionEntry::Usage(entry) => {
                add(format!("{}/{}", entry.provider, entry.model), &entry.usage);
            }
            SessionEntry::BranchSummary(entry) => {
                if let Some(usage) = &entry.usage {
                    add("Tools/summaries".to_string(), usage);
                }
            }
            SessionEntry::Compaction(entry) => {
                if let Some(usage) = &entry.usage {
                    add("Tools/summaries".to_string(), usage);
                }
            }
            _ => {}
        }
    }

    let mut breakdown: Vec<UsageCostBreakdownEntry> = totals_by_key
        .into_iter()
        .map(|(key, totals)| UsageCostBreakdownEntry {
            key,
            cost: totals.cost,
            tokens: totals.tokens(),
        })
        .filter(|entry| entry.cost > 0.0 || entry.tokens > 0)
        .collect();
    breakdown.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    breakdown
}

/// The minimum gap between the stats and the model (`footer.ts:231`).
const MIN_PADDING: usize = 2;

/// `sanitizeStatusText` (`footer.ts:14-20`): newlines, tabs, and carriage
/// returns become spaces, runs of spaces collapse, and the edges trim — a
/// single-line status.
fn sanitize_status_text(text: &str) -> String {
    let flattened: String = text
        .chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut out = String::with_capacity(flattened.len());
    let mut in_space = false;
    for c in flattened.chars() {
        if c == ' ' {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out.trim().to_string()
}

/// The data the footer renders, gathered by the interactive loop.
#[derive(Debug, Clone, Default)]
pub struct FooterData {
    pub cwd: String,
    pub git_branch: Option<String>,
    pub session_name: Option<String>,
    pub totals: UsageTotals,
    /// Cache-hit percentage of the latest prompt (0-100).
    pub latest_cache_hit_rate: Option<f64>,
    /// Context usage: percent (0-100) and the window that applies.
    pub context_percent: Option<f64>,
    pub context_window: u64,
    pub model_id: String,
    pub model_provider: String,
    pub model_supports_reasoning: bool,
    pub thinking_level: String,
    /// Unique providers with available models (`getAvailableProviderCount`,
    /// `footer-data-provider.ts:159-167`, fed by `updateAvailableProviderCount`,
    /// `interactive-mode.ts:5444-5451`).
    pub available_provider_count: usize,
    /// Under a virtual selection, the physical model the latest response used
    /// (`session.routedModel`, `agent-session.ts:1538-1543`).
    pub routed_model_id: Option<String>,
    /// The routed response's thinking level, when one was recorded.
    pub routed_thinking_level: Option<String>,
    pub auto_compact_enabled: bool,
    /// Extension status texts set via `ctx.ui.setStatus()` —
    /// `FooterDataProvider.extensionStatuses` (`footer-data-provider.ts:103`,
    /// `:135-137`, `:146-157`). Upstream keeps an insertion-ordered `Map` and
    /// sorts per render by `localeCompare` (`footer.ts:292-293`); the port's
    /// `BTreeMap` orders by key instead — identical for the ASCII keys
    /// extensions use, and the case-folded fallback stands in for the locale
    /// collation the port carries no ICU for.
    pub extension_statuses: std::collections::BTreeMap<String, String>,
}

/// `FooterComponent` (`footer.ts:62-302`).
pub struct FooterComponent {
    pub data: FooterData,
}

/// The stats line's right cell (`footer.ts:233-255`): the model id, plus
/// ` • thinking <level>` when the model supports reasoning, the routed
/// virtual model's ` → id[ • level]`, and a `(provider)` prefix when more
/// than one provider has available models and the cell still fits.
/// `stats_left_width` is the already-truncated left cell's width.
fn stats_right_side(data: &FooterData, stats_left_width: usize, width: usize) -> String {
    // The model name (`footer.ts:220`).
    let model_name = if data.model_id.is_empty() {
        "no-model".to_string()
    } else {
        data.model_id.clone()
    };
    // The thinking level indicator (`footer.ts:233-239`).
    let mut right_side_without_provider = model_name.clone();
    if data.model_supports_reasoning {
        let level = if data.thinking_level.is_empty() {
            "off"
        } else {
            data.thinking_level.as_str()
        };
        right_side_without_provider = if level == "off" {
            format!("{model_name} • thinking off")
        } else {
            format!("{model_name} • {level}")
        };
    }
    // A virtual model routes each request; show where the latest response
    // went (`footer.ts:240-245`; an empty level is falsy in TS).
    if let Some(routed_id) = &data.routed_model_id {
        let level = data
            .routed_thinking_level
            .as_deref()
            .filter(|level| !level.is_empty())
            .map_or(String::new(), |level| format!(" • {level}"));
        right_side_without_provider = format!("{right_side_without_provider} → {routed_id}{level}");
    }
    // The provider in parentheses when there are multiple providers and
    // there's enough room (`footer.ts:247-255`).
    let mut right_side = right_side_without_provider.clone();
    if data.available_provider_count > 1 && !data.model_id.is_empty() {
        right_side = format!("({}) {right_side_without_provider}", data.model_provider);
        if stats_left_width + MIN_PADDING + visible_width(&right_side) > width {
            // Too wide, fall back.
            right_side = right_side_without_provider;
        }
    }
    right_side
}

impl FooterComponent {
    pub fn new(data: FooterData) -> Self {
        FooterComponent { data }
    }

    /// `render` (`footer.ts:157-301`): `[pwdLine, statsLine]`, plus the
    /// extension-status line when one is set (`footer.ts:289-298`).
    #[allow(clippy::too_many_lines)]
    pub fn render_lines(&self, width: usize) -> Vec<String> {
        let theme = theme::theme();
        let data = &self.data;

        let mut pwd = format_cwd_for_footer(&data.cwd, std::env::var("HOME").ok().as_deref());
        if let Some(branch) = &data.git_branch {
            pwd = format!("{pwd} ({branch})");
        }
        if let Some(session_name) = &data.session_name {
            pwd = format!("{pwd} • {session_name}");
        }

        let totals = data.totals;
        let mut stats_parts: Vec<String> = Vec::new();
        if totals.input > 0 {
            stats_parts.push(format!("↑{}", format_tokens(totals.input)));
        }
        if totals.output > 0 {
            stats_parts.push(format!("↓{}", format_tokens(totals.output)));
        }
        if totals.cache_read > 0 {
            stats_parts.push(format!("R{}", format_tokens(totals.cache_read)));
        }
        if totals.cache_write > 0 {
            stats_parts.push(format!("W{}", format_tokens(totals.cache_write)));
        }
        if (totals.cache_read > 0 || totals.cache_write > 0)
            && let Some(rate) = data.latest_cache_hit_rate
        {
            stats_parts.push(format!("CH{rate:.1}%"));
        }
        if totals.cost > 0.0 {
            stats_parts.push(format!("${:.3}", totals.cost));
        }

        // The context percentage, colored by usage (`footer.ts:198-212`).
        let auto_indicator = if data.auto_compact_enabled {
            " (auto)"
        } else {
            ""
        };
        let context_window = data.context_window;
        let context_percent_display = match data.context_percent {
            None => format!("?/{}{auto_indicator}", format_tokens(context_window)),
            Some(percent) => {
                format!(
                    "{percent:.1}%/{}{auto_indicator}",
                    format_tokens(context_window)
                )
            }
        };
        let context_percent_value = data.context_percent.unwrap_or(0.0);
        let context_percent_str = if context_percent_value > 90.0 {
            theme.fg(ThemeColor::Error, &context_percent_display)
        } else if context_percent_value > 70.0 {
            theme.fg(ThemeColor::Warning, &context_percent_display)
        } else {
            context_percent_display.clone()
        };
        stats_parts.push(context_percent_str);

        let mut stats_left = stats_parts.join(" ");

        // The left cell truncates before the right side is built, so the
        // provider prefix knows the room left (`footer.ts:222-231`).
        let mut stats_left_width = visible_width(&stats_left);
        if stats_left_width > width {
            stats_left = truncate_to_width(&stats_left, width, "...", false);
            stats_left_width = visible_width(&stats_left);
        }

        // The model on the right, with the thinking level, the routed virtual
        // model, and the provider prefix (`footer.ts:233-255`).
        let right_side = stats_right_side(data, stats_left_width, width);

        let right_side_width = visible_width(&right_side);
        let stats_line = if stats_left_width + MIN_PADDING + right_side_width <= width {
            let padding = " ".repeat(width - stats_left_width - right_side_width);
            format!("{stats_left}{padding}{right_side}")
        } else {
            let available_for_right = width.saturating_sub(stats_left_width + MIN_PADDING);
            if available_for_right > 0 {
                let truncated_right =
                    truncate_to_width(&right_side, available_for_right, "", false);
                let truncated_right_width = visible_width(&truncated_right);
                let padding =
                    " ".repeat(width.saturating_sub(stats_left_width + truncated_right_width));
                format!("{stats_left}{padding}{truncated_right}")
            } else {
                stats_left.clone()
            }
        };

        // Dim each part separately: the colored context percent ends with a
        // reset that would clear an outer dim wrapper (`footer.ts:279-284`).
        let dim_stats_left = theme.fg(ThemeColor::Dim, &stats_left);
        let remainder = stats_line[stats_left.len().min(stats_line.len())..].to_string();
        let dim_remainder = theme.fg(ThemeColor::Dim, &remainder);

        let pwd_line = truncate_to_width(
            &theme.fg(ThemeColor::Dim, &pwd),
            width,
            &theme.fg(ThemeColor::Dim, "..."),
            false,
        );
        let mut lines = vec![pwd_line, format!("{dim_stats_left}{dim_remainder}")];

        // `footer.ts:289-298`: the extension statuses ride a single extra
        // line — only when one is set — sorted by key, each value
        // sanitized, joined with spaces, truncated to the width with the
        // dim ellipsis like the other footer lines.
        if !data.extension_statuses.is_empty() {
            let extension_line = data
                .extension_statuses
                .values()
                .map(|text| sanitize_status_text(text))
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(truncate_to_width(
                &extension_line,
                width,
                &theme.fg(ThemeColor::Dim, "..."),
                false,
            ));
        }

        lines
    }
}

impl pi_tui::component::Component for FooterComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.render_lines(width)
    }

    /// The orchestrator refreshes the stats through the tree downcast.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_format_matches_upstream() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1500), "1.5k");
        assert_eq!(format_tokens(15_000), "15k");
        assert_eq!(format_tokens(2_500_000), "2.5M");
        assert_eq!(format_tokens(15_000_000), "15M");
    }

    #[test]
    fn cwd_format_shortens_inside_home() {
        assert_eq!(
            format_cwd_for_footer("/home/u/proj", Some("/home/u")),
            "~/proj"
        );
        assert_eq!(format_cwd_for_footer("/home/u", Some("/home/u")), "~");
        assert_eq!(format_cwd_for_footer("/etc", Some("/home/u")), "/etc");
        assert_eq!(format_cwd_for_footer("/etc", None), "/etc");
    }

    #[test]
    fn stats_line_splits_and_right_aligns() {
        let footer = FooterComponent::new(FooterData {
            cwd: "/tmp/proj".to_string(),
            git_branch: Some("main".to_string()),
            totals: UsageTotals {
                input: 1500,
                output: 300,
                cache_read: 12_000,
                cache_write: 500,
                cost: 0.0125,
            },
            latest_cache_hit_rate: Some(88.8),
            context_percent: Some(12.3),
            context_window: 200_000,
            model_id: "test-model".to_string(),
            model_provider: "test".to_string(),
            model_supports_reasoning: true,
            thinking_level: "medium".to_string(),
            auto_compact_enabled: true,
            ..FooterData::default()
        });
        let lines = footer.render_lines(80);
        assert_eq!(lines.len(), 2);
        let plain = pi_tui::ansi::strip_terminal_sequences(&lines[0]);
        assert_eq!(plain, "/tmp/proj (main)");
        let plain = pi_tui::ansi::strip_terminal_sequences(&lines[1]);
        assert!(plain.contains("↑1.5k"), "{plain}");
        assert!(plain.contains("↓300"), "{plain}");
        assert!(plain.contains("R12k"), "{plain}");
        assert!(plain.contains("W500"), "{plain}");
        assert!(plain.contains("CH88.8%"), "{plain}");
        assert!(plain.contains("$0.013"), "{plain}");
        assert!(plain.contains("12.3%/200k (auto)"), "{plain}");
        assert!(plain.ends_with("test-model • medium"), "{plain}");
    }

    /// A reasoning model at `off` spells the level out; a non-reasoning model
    /// shows the bare id (`footer.ts:233-239`).
    #[test]
    fn thinking_level_display() {
        let data = FooterData {
            model_id: "test-model".to_string(),
            model_provider: "test".to_string(),
            ..FooterData::default()
        };
        let mut footer = FooterComponent::new(FooterData {
            model_supports_reasoning: true,
            thinking_level: "off".to_string(),
            ..data.clone()
        });
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(120)[1]);
        assert!(plain.ends_with("test-model • thinking off"), "{plain}");
        footer = FooterComponent::new(FooterData {
            model_supports_reasoning: true,
            thinking_level: "high".to_string(),
            ..data.clone()
        });
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(120)[1]);
        assert!(plain.ends_with("test-model • high"), "{plain}");
        // An empty level reads as `off` (`state.thinkingLevel || "off"`).
        footer = FooterComponent::new(FooterData {
            model_supports_reasoning: true,
            ..data.clone()
        });
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(120)[1]);
        assert!(plain.ends_with("test-model • thinking off"), "{plain}");
        // No reasoning support: the bare model id.
        footer = FooterComponent::new(FooterData { ..data });
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(120)[1]);
        assert!(plain.ends_with("test-model"), "{plain}");
    }

    /// With more than one available provider the model cell gains the
    /// `(provider)` prefix (`footer.ts:247-255`).
    #[test]
    fn provider_prefix_shows_when_it_fits() {
        let data = FooterData {
            model_id: "test-model".to_string(),
            model_provider: "anthropic".to_string(),
            model_supports_reasoning: true,
            thinking_level: "medium".to_string(),
            auto_compact_enabled: true,
            ..FooterData::default()
        };
        let footer = FooterComponent::new(FooterData {
            available_provider_count: 3,
            ..data.clone()
        });
        // Left cell "?/0 (auto)" (10) + 2 padding + "(anthropic) test-model
        // • medium" (31) needs 43 columns; 60 leaves room for the prefix.
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(60)[1]);
        assert!(
            plain.ends_with("(anthropic) test-model • medium"),
            "{plain}"
        );
        // One provider: no prefix.
        let footer = FooterComponent::new(FooterData {
            available_provider_count: 1,
            ..data
        });
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(60)[1]);
        assert!(plain.ends_with("test-model • medium"), "{plain}");
        assert!(!plain.contains("(anthropic)"), "{plain}");
    }

    /// The prefix falls back to the unprefixed cell when it would push the
    /// line past the width (`footer.ts:251-254`).
    #[test]
    fn provider_prefix_falls_back_when_too_wide() {
        let footer = FooterComponent::new(FooterData {
            model_id: "test-model".to_string(),
            model_provider: "anthropic".to_string(),
            model_supports_reasoning: true,
            thinking_level: "medium".to_string(),
            available_provider_count: 3,
            auto_compact_enabled: true,
            ..FooterData::default()
        });
        // Left cell (10) + 2 padding + the prefixed cell (31) = 43 > 36, while
        // the unprefixed cell (19) fits at 31 <= 36.
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(36)[1]);
        assert!(plain.ends_with("test-model • medium"), "{plain}");
        assert!(!plain.contains("(anthropic)"), "{plain}");
    }

    /// A virtual selection shows the physical model the latest response used
    /// (`footer.ts:240-245`), before the provider prefix is applied.
    #[test]
    fn routed_virtual_model_appends_the_physical_model() {
        let data = FooterData {
            model_id: "acme/pool".to_string(),
            model_provider: "acme".to_string(),
            model_supports_reasoning: true,
            thinking_level: "high".to_string(),
            ..FooterData::default()
        };
        let footer = FooterComponent::new(FooterData {
            routed_model_id: Some("claude-opus-4-5".to_string()),
            routed_thinking_level: Some("medium".to_string()),
            ..data.clone()
        });
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(120)[1]);
        assert!(
            plain.ends_with("acme/pool • high → claude-opus-4-5 • medium"),
            "{plain}"
        );
        // Without a recorded level the arrow ends at the model id.
        let footer = FooterComponent::new(FooterData {
            routed_model_id: Some("claude-opus-4-5".to_string()),
            ..data.clone()
        });
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(120)[1]);
        assert!(
            plain.ends_with("acme/pool • high → claude-opus-4-5"),
            "{plain}"
        );
        // The prefix wraps the routed cell when there is room.
        let footer = FooterComponent::new(FooterData {
            routed_model_id: Some("claude-opus-4-5".to_string()),
            available_provider_count: 2,
            ..data
        });
        let plain = pi_tui::ansi::strip_terminal_sequences(&footer.render_lines(120)[1]);
        assert!(
            plain.ends_with("(acme) acme/pool • high → claude-opus-4-5"),
            "{plain}"
        );
    }

    #[test]
    fn missing_context_shows_the_question_mark() {
        let footer = FooterComponent::new(FooterData {
            cwd: "/tmp".to_string(),
            context_percent: None,
            context_window: 0,
            model_id: "m".to_string(),
            auto_compact_enabled: true,
            ..FooterData::default()
        });
        let lines = footer.render_lines(80);
        let plain = pi_tui::ansi::strip_terminal_sequences(&lines[1]);
        assert!(plain.contains("?/0 (auto)"), "{plain}");
    }

    /// `footer.ts:289-298`: statuses ride a third line, sorted by key,
    /// sanitized, joined with spaces — and the line exists only when a
    /// status is set.
    #[test]
    fn extension_statuses_render_sorted_sanitized_and_only_when_set() {
        let plain = FooterData {
            cwd: "/tmp".to_string(),
            model_id: "m".to_string(),
            ..FooterData::default()
        };
        // No statuses: exactly the two base lines.
        let footer = FooterComponent::new(plain.clone());
        assert_eq!(footer.render_lines(80).len(), 2);
        // Two statuses render sorted by key ("beta" before "alpha-sort" is
        // false — alphabetical) with their values joined.
        let footer = FooterComponent::new(FooterData {
            extension_statuses: [
                ("zeta".to_string(), "second status".to_string()),
                ("alpha".to_string(), "first\tstatus".to_string()),
            ]
            .into_iter()
            .collect(),
            ..plain.clone()
        });
        let lines = footer.render_lines(80);
        assert_eq!(lines.len(), 3, "the status line is appended");
        let plain_line = pi_tui::ansi::strip_terminal_sequences(&lines[2]);
        // Sorted by key: alpha's (sanitized: tab → space) first.
        assert_eq!(plain_line, "first status second status");
    }

    /// `footer.ts:14-20`: the sanitizer replaces CR/LF/tab runs with one
    /// space and trims; `footer.ts:297` truncates a wide line with the dim
    /// ellipsis.
    #[test]
    fn status_lines_sanitize_and_truncate() {
        assert_eq!(sanitize_status_text("a\r\nb\tc"), "a b c");
        assert_eq!(sanitize_status_text("  a   b  "), "a b");
        assert_eq!(sanitize_status_text("\r\n\t"), "");
        let footer = FooterComponent::new(FooterData {
            extension_statuses: [("wide".to_string(), "x".repeat(200))]
                .into_iter()
                .collect(),
            ..FooterData::default()
        });
        let width = 40;
        let lines = footer.render_lines(width);
        let plain_line = pi_tui::ansi::strip_terminal_sequences(&lines[2]);
        assert_eq!(
            pi_tui::ansi::visible_width(&plain_line),
            width,
            "{plain_line}"
        );
        assert!(plain_line.ends_with("..."), "{plain_line}");
    }
}
