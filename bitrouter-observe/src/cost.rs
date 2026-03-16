//! Cost calculation from token usage and pricing rates.

use bitrouter_core::models::language::usage::LanguageModelUsage;

/// Per-million-token pricing rates for a model.
#[derive(Debug, Clone)]
pub struct Pricing {
    /// Cost per million non-cached input tokens.
    pub input_no_cache: f64,
    /// Cost per million cache-read input tokens.
    pub input_cache_read: f64,
    /// Cost per million cache-write input tokens.
    pub input_cache_write: f64,
    /// Cost per million text output tokens.
    pub output_text: f64,
    /// Cost per million reasoning output tokens.
    pub output_reasoning: f64,
}

impl Default for Pricing {
    fn default() -> Self {
        Self {
            input_no_cache: 0.0,
            input_cache_read: 0.0,
            input_cache_write: 0.0,
            output_text: 0.0,
            output_reasoning: 0.0,
        }
    }
}

const PER_MILLION: f64 = 1_000_000.0;

/// Calculates the cost of a request from token usage and pricing rates.
///
/// For input tokens: if granular buckets (`no_cache`, `cache_read`,
/// `cache_write`) are available they are used with their respective rates.
/// Otherwise falls back to `total * input_no_cache`.
///
/// For output tokens: if granular buckets (`text`, `reasoning`) are available
/// they are used. Otherwise falls back to `total * output_text`.
pub fn calculate_cost(usage: &LanguageModelUsage, pricing: &Pricing) -> f64 {
    let input_cost = {
        let has_granular = usage.input_tokens.no_cache.is_some()
            || usage.input_tokens.cache_read.is_some()
            || usage.input_tokens.cache_write.is_some();

        if has_granular {
            let no_cache = usage.input_tokens.no_cache.unwrap_or(0) as f64;
            let cache_read = usage.input_tokens.cache_read.unwrap_or(0) as f64;
            let cache_write = usage.input_tokens.cache_write.unwrap_or(0) as f64;
            (no_cache * pricing.input_no_cache
                + cache_read * pricing.input_cache_read
                + cache_write * pricing.input_cache_write)
                / PER_MILLION
        } else if let Some(total) = usage.input_tokens.total {
            total as f64 * pricing.input_no_cache / PER_MILLION
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
            (text * pricing.output_text + reasoning * pricing.output_reasoning) / PER_MILLION
        } else if let Some(total) = usage.output_tokens.total {
            total as f64 * pricing.output_text / PER_MILLION
        } else {
            0.0
        }
    };

    input_cost + output_cost
}

#[cfg(test)]
mod tests {
    use bitrouter_core::models::language::usage::{
        LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage,
    };

    use super::*;

    fn test_pricing() -> Pricing {
        Pricing {
            input_no_cache: 2.50,
            input_cache_read: 1.25,
            input_cache_write: 3.75,
            output_text: 10.00,
            output_reasoning: 15.00,
        }
    }

    #[test]
    fn granular_input_and_output() {
        let usage = LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(3000),
                no_cache: Some(1000),
                cache_read: Some(1500),
                cache_write: Some(500),
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(600),
                text: Some(400),
                reasoning: Some(200),
            },
            raw: None,
        };

        let cost = calculate_cost(&usage, &test_pricing());
        // input: (1000*2.50 + 1500*1.25 + 500*3.75) / 1_000_000
        //      = (2500 + 1875 + 1875) / 1_000_000 = 6250 / 1_000_000 = 0.00625
        // output: (400*10.0 + 200*15.0) / 1_000_000
        //       = (4000 + 3000) / 1_000_000 = 7000 / 1_000_000 = 0.007
        let expected = 0.00625 + 0.007;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn fallback_to_total_when_no_granular() {
        let usage = LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(2000),
                no_cache: None,
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(500),
                text: None,
                reasoning: None,
            },
            raw: None,
        };

        let cost = calculate_cost(&usage, &test_pricing());
        // input: 2000 * 2.50 / 1_000_000 = 0.005
        // output: 500 * 10.0 / 1_000_000 = 0.005
        let expected = 0.005 + 0.005;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn zero_tokens_yields_zero_cost() {
        let usage = LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: None,
                no_cache: None,
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: None,
                text: None,
                reasoning: None,
            },
            raw: None,
        };

        let cost = calculate_cost(&usage, &test_pricing());
        assert!((cost - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn zero_pricing_yields_zero_cost() {
        let usage = LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(10000),
                no_cache: Some(10000),
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(5000),
                text: Some(5000),
                reasoning: None,
            },
            raw: None,
        };

        let cost = calculate_cost(&usage, &Pricing::default());
        assert!((cost - 0.0).abs() < f64::EPSILON);
    }
}
