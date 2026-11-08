//! Cost calculation from usage and catalog prices (port of TS
//! `calculateCost` in `models.ts`).

use crate::types::{ModelCost, Usage};

/// Prices are per million tokens. `usage.cost.total` sums the four measured
/// components; provider-reported billed cost handling happens in adapters.
#[allow(clippy::cast_precision_loss)] // token counts far below 2^53 in practice
pub fn calculate_cost(usage: &mut Usage, cost: &ModelCost) {
    usage.cost.input = usage.input as f64 * cost.input / 1_000_000.0;
    usage.cost.output = usage.output as f64 * cost.output / 1_000_000.0;
    usage.cost.cache_read = usage.cache_read as f64 * cost.cache_read / 1_000_000.0;
    usage.cost.cache_write = usage.cache_write as f64 * cost.cache_write / 1_000_000.0;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    usage.cost.total_from_provider = None;
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
