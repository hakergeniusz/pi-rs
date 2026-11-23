//! `--list-models` (`cli/list-models.ts`): the six-column aligned table,
//! human-readable token counts, fuzzy search and sort.
//!
//! Ground truth: `formatTokenCount` (`:13-24`), `listModels` (`:27-115`).
//! The filter uses the port's 1:1 `fuzzyFilter` (`pi_tui::fuzzy::fuzzy_filter`)
//! over `"<provider> <id>"`, then re-sorts by provider/model id exactly like
//! upstream's `localeCompare` pair.

use pi_ai::types::{Model, ModelInputModality};

/// `formatTokenCount` (`list-models.ts:13-24`): `200000 -> "200K"`,
/// `1000000 -> "1M"`, with one decimal for non-integral multiples.
fn format_token_count(count: u64) -> String {
    if count >= 1_000_000 {
        let millions = count as f64 / 1_000_000.0;
        if millions.fract() == 0.0 {
            format!("{millions:.0}M")
        } else {
            format!("{millions:.1}M")
        }
    } else if count >= 1_000 {
        let thousands = count as f64 / 1_000.0;
        if thousands.fract() == 0.0 {
            format!("{thousands:.0}K")
        } else {
            format!("{thousands:.1}K")
        }
    } else {
        count.to_string()
    }
}

const HEADERS: [&str; 6] = [
    "provider", "model", "context", "max-out", "thinking", "images",
];

struct Row {
    cells: [String; 6],
}

/// `--list-models [search]` (`main.ts:948-952`). Composition errors surface
/// with upstream's warning prefix, and an empty catalog uses the local
/// equivalent of `formatNoModelsAvailableMessage()`.
pub fn print_models(search: Option<&str>) {
    for error in crate::models_state::errors() {
        eprintln!("Warning: errors loading models.json:\n{error}");
    }
    let mut models = crate::models_state::all();
    if models.is_empty() {
        println!(
            "No models available. Use `{APP_NAME} auth login` to configure a provider, or set a provider API key env var.",
            APP_NAME = crate::config::APP_NAME
        );
        return;
    }
    if let Some(pattern) = search.filter(|pattern| !pattern.is_empty()) {
        models = pi_tui::fuzzy::fuzzy_filter(&models, pattern, |model: &Model| {
            format!("{} {}", model.provider, model.id)
        });
    }
    if models.is_empty() {
        println!("No models matching \"{}\"", search.unwrap_or_default());
        return;
    }
    models.sort_by(|a, b| a.provider.cmp(&b.provider).then_with(|| a.id.cmp(&b.id)));
    let rows: Vec<Row> = models
        .iter()
        .map(|model| Row {
            cells: [
                model.provider.clone(),
                model.id.clone(),
                format_token_count(model.context_window),
                format_token_count(model.max_tokens),
                yes_no(model.reasoning),
                yes_no(model.input.contains(&ModelInputModality::Image)),
            ],
        })
        .collect();
    let mut widths = [0usize; 6];
    for (index, header) in HEADERS.iter().enumerate() {
        widths[index] = header.len();
    }
    for row in &rows {
        for (index, cell) in row.cells.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let render = |cells: [&str; 6]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| format!("{cell:<width$}", width = widths[index]))
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!("{}", render(HEADERS));
    for row in &rows {
        let cells = row.cells.each_ref().map(String::as_str);
        println!("{}", render(cells));
    }
}

fn yes_no(value: bool) -> String {
    if value { "yes" } else { "no" }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_match_upstream() {
        assert_eq!(format_token_count(200_000), "200K");
        assert_eq!(format_token_count(1_000_000), "1M");
        assert_eq!(format_token_count(1_500_000), "1.5M");
        assert_eq!(format_token_count(1_234), "1.2K");
        assert_eq!(format_token_count(999), "999");
    }
}
