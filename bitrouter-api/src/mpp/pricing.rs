//! Cost-to-payment-unit conversion for MPP deduction.
//!
//! Converts USD costs (from token usage and model pricing) into stablecoin
//! micro-units suitable for on-chain or session-channel deductions. Assumes
//! a 1:1 stablecoin peg with 6 decimal places (e.g. USDC).

use bitrouter_config::ModelPricing;
use bitrouter_core::models::language::usage::LanguageModelUsage;

const PER_MILLION: f64 = 1_000_000.0;

/// Trait for looking up per-model pricing.
///
/// Implemented for `bitrouter_config::ConfigRoutingTable` to allow
/// MPP handlers to compute per-request costs without tight coupling.
pub trait PricingLookup {
    /// Returns the per-million-token pricing for a model under a given provider.
    fn model_pricing(&self, provider: &str, model_id: &str) -> ModelPricing;
}

impl PricingLookup for bitrouter_config::ConfigRoutingTable {
    fn model_pricing(&self, provider: &str, model_id: &str) -> ModelPricing {
        self.model_pricing(provider, model_id)
    }
}

impl<T: PricingLookup> PricingLookup for bitrouter_core::routers::dynamic::DynamicRoutingTable<T> {
    fn model_pricing(&self, provider: &str, model_id: &str) -> ModelPricing {
        self.read_inner().model_pricing(provider, model_id)
    }
}

/// Calculates the cost in USD from token usage and per-million-token pricing.
///
/// Mirrors `bitrouter-observe::cost::calculate_cost` but uses the
/// `ModelPricing` type from `bitrouter-config` directly, avoiding a
/// heavy crate dependency.
pub fn calculate_usage_cost(usage: &LanguageModelUsage, pricing: &ModelPricing) -> f64 {
    let input_cost = {
        let has_granular = usage.input_tokens.no_cache.is_some()
            || usage.input_tokens.cache_read.is_some()
            || usage.input_tokens.cache_write.is_some();

        if has_granular {
            let no_cache = usage.input_tokens.no_cache.unwrap_or(0) as f64;
            let cache_read = usage.input_tokens.cache_read.unwrap_or(0) as f64;
            let cache_write = usage.input_tokens.cache_write.unwrap_or(0) as f64;
            (no_cache * pricing.input_tokens.no_cache
                + cache_read * pricing.input_tokens.cache_read
                + cache_write * pricing.input_tokens.cache_write)
                / PER_MILLION
        } else if let Some(total) = usage.input_tokens.total {
            total as f64 * pricing.input_tokens.no_cache / PER_MILLION
        } else {
            0.0
        }
    };

    let output_cost = {
        let has_granular =
            usage.output_tokens.text.is_some() || usage.output_tokens.reasoning.is_some();

        if has_granular {
            let text = usage.output_tokens.text.unwrap_or(0) as f64;
            let reasoning = usage.output_tokens.reasoning.unwrap_or(0) as f64;
            (text * pricing.output_tokens.text + reasoning * pricing.output_tokens.reasoning)
                / PER_MILLION
        } else if let Some(total) = usage.output_tokens.total {
            total as f64 * pricing.output_tokens.text / PER_MILLION
        } else {
            0.0
        }
    };

    input_cost + output_cost
}

/// Converts a USD cost to stablecoin micro-units (6 decimal places).
///
/// Assumes a 1:1 stablecoin peg (e.g. 1 USDC = 1 USD). Returns the cost
/// in the smallest denomination (1 micro-unit = 0.000001 USD).
///
/// Negative costs are clamped to zero.
pub fn cost_to_micro_units(cost_usd: f64) -> u128 {
    if cost_usd <= 0.0 {
        return 0;
    }
    (cost_usd * 1_000_000.0).round() as u128
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_cost() {
        assert_eq!(cost_to_micro_units(0.0), 0);
    }

    #[test]
    fn negative_cost() {
        assert_eq!(cost_to_micro_units(-1.5), 0);
    }

    #[test]
    fn one_dollar() {
        assert_eq!(cost_to_micro_units(1.0), 1_000_000);
    }

    #[test]
    fn fractional_cost() {
        // $0.000015 per token * 1000 tokens = $0.015
        assert_eq!(cost_to_micro_units(0.015), 15_000);
    }

    #[test]
    fn sub_micro_unit_rounds() {
        // $0.0000004 → 0.4 micro-units → rounds to 0
        assert_eq!(cost_to_micro_units(0.0000004), 0);
        // $0.0000006 → 0.6 micro-units → rounds to 1
        assert_eq!(cost_to_micro_units(0.0000006), 1);
    }

    #[test]
    fn large_cost() {
        // $100.00 = 100_000_000 micro-units
        assert_eq!(cost_to_micro_units(100.0), 100_000_000);
    }
}
