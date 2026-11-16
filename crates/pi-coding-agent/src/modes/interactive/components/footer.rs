//! `FooterComponent` — the two status lines at the bottom of the dock: the
//! working directory (with git branch and session name) and the token/cost/
//! context stats with the model on the right.
//!
//! Ground truth: `modes/interactive/components/footer.ts`.

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
    pub auto_compact_enabled: bool,
}

/// `FooterComponent` (`footer.ts:62-302`).
pub struct FooterComponent {
    pub data: FooterData,
}

/// The stats line's right cell: model id, plus ` • thinking <level>` when the
/// model supports reasoning (`footer.ts:219-245`).
fn stats_right_side(data: &FooterData) -> String {
    let base = if data.model_id.is_empty() {
        "no-model".to_string()
    } else {
        data.model_id.clone()
    };
    if !data.model_supports_reasoning {
        return base;
    }
    if data.thinking_level.is_empty() || data.thinking_level == "off" {
        format!("{base} • thinking off")
    } else {
        format!("{base} • {}", data.thinking_level)
    }
}

impl FooterComponent {
    pub fn new(data: FooterData) -> Self {
        FooterComponent { data }
    }

    /// `render` (`footer.ts:157-301`): `[pwdLine, statsLine]`.
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

        // The model on the right, with the thinking level when supported
        // (`footer.ts:219-245`).
        let right_side = stats_right_side(data);

        let mut stats_left_width = visible_width(&stats_left);
        if stats_left_width > width {
            stats_left = truncate_to_width(&stats_left, width, "...", false);
            stats_left_width = visible_width(&stats_left);
        }

        let min_padding = 2usize;
        let right_side_width = visible_width(&right_side);
        let stats_line = if stats_left_width + min_padding + right_side_width <= width {
            let padding = " ".repeat(width - stats_left_width - right_side_width);
            format!("{stats_left}{padding}{right_side}")
        } else {
            let available_for_right = width.saturating_sub(stats_left_width + min_padding);
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
        vec![pwd_line, format!("{dim_stats_left}{dim_remainder}")]
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
            session_name: None,
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
}
