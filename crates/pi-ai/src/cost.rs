//! Cost calculation from usage and catalog prices (port of TS
//! `calculateCost` in `models.ts:1279-1299`). Request-wide tiers
//! (`providers/model-schema.ts:61-74`) and the 1h cache-write split are applied
//! exactly as upstream does.

use crate::types::{ModelCost, Usage};

/// The four rates shared by base costs and tiers.
#[derive(Debug, Clone, Copy)]
struct Rates {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

impl From<&ModelCost> for Rates {
    fn from(cost: &ModelCost) -> Self {
        Self {
            input: cost.input,
            output: cost.output,
            cache_read: cost.cache_read,
            cache_write: cost.cache_write,
        }
    }
}

/// Prices are per million tokens. `usage.cost.total` sums the four measured
/// components; provider-reported billed cost handling happens in adapters.
#[allow(clippy::cast_precision_loss)] // token counts far below 2^53 in practice
pub fn calculate_cost(usage: &mut Usage, cost: &ModelCost) {
    // Highest matching input threshold applies to the *whole* request
    // (`models.ts:1280-1288`): cache reads/writes count toward the total.
    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let mut matched: Option<&crate::types::ModelCostTier> = None;
    for tier in cost.tiers.iter().flatten() {
        if input_tokens > tier.input_tokens_above
            && matched.is_none_or(|best| tier.input_tokens_above > best.input_tokens_above)
        {
            matched = Some(tier);
        }
    }

    let rates = match matched {
        Some(tier) => Rates {
            input: tier.input,
            output: tier.output,
            cache_read: tier.cache_read,
            cache_write: tier.cache_write,
        },
        None => Rates::from(cost),
    };

    // Anthropic charges 2x base input for 1h cache writes unless the catalog
    // supplies an explicit rate (`models.ts:1290-1296`).
    let long_write = usage.cache_write_1h.unwrap_or(0).min(usage.cache_write);
    let short_write = usage.cache_write - long_write;
    let long_rate = cost.cache_write_1h.unwrap_or(rates.input * 2.0);

    usage.cost.input = rates.input / 1_000_000.0 * usage.input as f64;
    usage.cost.output = rates.output / 1_000_000.0 * usage.output as f64;
    usage.cost.cache_read = rates.cache_read / 1_000_000.0 * usage.cache_read as f64;
    usage.cost.cache_write =
        (rates.cache_write * short_write as f64 + long_rate * long_write as f64) / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    usage.cost.total_from_provider = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ModelCostTier;

    #[test]
    fn computes_per_million_costs() {
        let mut usage = Usage {
            input: 1_000_000,
            output: 500_000,
            cache_read: 0,
            cache_write: 0,
            ..Default::default()
        };
        calculate_cost(
            &mut usage,
            &ModelCost {
                input: 3.0,
                output: 15.0,
                ..Default::default()
            },
        );
        assert!((usage.cost.input - 3.0).abs() < 1e-9);
        assert!((usage.cost.output - 7.5).abs() < 1e-9);
        assert!((usage.cost.total - 10.5).abs() < 1e-9);
    }

    #[test]
    fn highest_matching_tier_prices_the_whole_request() {
        let cost = ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            tiers: Some(vec![
                ModelCostTier {
                    input_tokens_above: 32_000,
                    input: 1.8,
                    output: 9.0,
                    cache_read: 0.2,
                    cache_write: 2.0,
                },
                ModelCostTier {
                    input_tokens_above: 128_000,
                    input: 6.0,
                    output: 18.0,
                    cache_read: 0.4,
                    cache_write: 5.0,
                },
            ]),
            ..Default::default()
        };
        // 100k input tokens: only the 32k tier matches, and it replaces *all*
        // base rates including output.
        let mut usage = Usage {
            input: 100_000,
            output: 10_000,
            ..Default::default()
        };
        calculate_cost(&mut usage, &cost);
        assert!((usage.cost.input - 0.18).abs() < 1e-9);
        assert!((usage.cost.output - 0.09).abs() < 1e-9);

        // Cache tokens count toward the threshold; at 130k the 128k tier wins.
        let mut usage = Usage {
            input: 100_000,
            cache_read: 30_000,
            output: 1_000,
            ..Default::default()
        };
        calculate_cost(&mut usage, &cost);
        assert!((usage.cost.input - 0.6).abs() < 1e-9);
        assert!((usage.cost.cache_read - 0.012).abs() < 1e-9);
    }

    #[test]
    fn splits_1h_cache_writes_at_twice_the_input_rate() {
        let cost = ModelCost {
            input: 3.0,
            output: 15.0,
            cache_write: 3.75,
            ..Default::default()
        };
        let mut usage = Usage {
            input: 0,
            output: 0,
            cache_write: 1_000_000,
            cache_write_1h: Some(400_000),
            ..Default::default()
        };
        calculate_cost(&mut usage, &cost);
        // 600k short writes at 3.75 + 400k long writes at 2 * 3.0.
        assert!((usage.cost.cache_write - (2.25 + 2.4)).abs() < 1e-9);
        assert!((usage.cost.total - 4.65).abs() < 1e-9);
    }

    #[test]
    fn deserializes_catalog_tiers() {
        let cost: ModelCost = serde_json::from_str(
            r#"{"input":1.2,"output":6,"cacheRead":0.1,"cacheWrite":1.5,
                "tiers":[{"inputTokensAbove":272000,"input":4,"output":15,
                          "cacheRead":0.4,"cacheWrite":5}]}"#,
        )
        .unwrap();
        let tier = &cost.tiers.as_ref().unwrap()[0];
        assert_eq!(tier.input_tokens_above, 272_000);
        assert!((tier.output - 15.0).abs() < 1e-9);
    }
}
