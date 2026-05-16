//! Pricing primitives shared across the BitRouter crate ecosystem.
//!
//! This module provides:
//! - [`FlatPricing`] — a generic "default + per-key overrides" pricing structure
//!   used for tool and agent invocation costs.
//! - [`calculate_cost`] — computes the USD cost of a model request from token
//!   usage and per-million-token pricing rates.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::models::language::usage::LanguageModelUsage;
use crate::routers::routing_table::ModelPricing;

/// Flat per-invocation pricing with optional per-key overrides.
///
/// Used for both MCP tool servers (keyed by tool name) and A2A agents
/// (keyed by method name). The generic field names allow `bitrouter-core`
/// to remain agnostic of the specific service type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlatPricing {
    /// Default cost per invocation (USD).
    #[serde(default, alias = "default_cost_per_call")]
    pub default: f64,
    /// Per-key cost overrides. Keys are service-specific identifiers
    /// (e.g. tool names, A2A method names).
    #[serde(default, alias = "tools", alias = "methods")]
    pub overrides: HashMap<String, f64>,
}

impl FlatPricing {
    /// Returns the cost for the given key, falling back to [`Self::default`].
    pub fn cost_for(&self, key: &str) -> f64 {
        self.overrides.get(key).copied().unwrap_or(self.default)
    }
}

const PER_MILLION: f64 = 1_000_000.0;

/// Calculates the USD cost of a model request from token usage and pricing.
///
/// Returns `None` when any token bucket with a nonzero count has no
/// matching rate. Treating missing rates as zero would silently undercharge
/// (a provider that omitted output pricing would bill all output tokens at
/// $0); callers must decide what to do with `None` — typically: log
/// `pricing_unavailable`, skip the debit, and surface a receipt with no
/// charge. Returns `Some(0.0)` when usage is empty or every nonzero bucket
/// has a rate of zero.
///
/// For input tokens: if granular buckets (`no_cache`, `cache_read`,
/// `cache_write`) are available they are used with their respective rates.
/// Otherwise falls back to `total * input_no_cache`.
///
/// For output tokens: if granular buckets (`text`, `reasoning`) are available
/// they are used. Otherwise falls back to `total * output_text`.
pub fn calculate_cost(usage: &LanguageModelUsage, pricing: &ModelPricing) -> Option<f64> {
    let input_cost = calculate_input_cost(usage, pricing)?;
    let output_cost = calculate_output_cost(usage, pricing)?;
    Some(input_cost + output_cost)
}

fn calculate_input_cost(usage: &LanguageModelUsage, pricing: &ModelPricing) -> Option<f64> {
    let has_granular = usage.input_tokens.no_cache.is_some()
        || usage.input_tokens.cache_read.is_some()
        || usage.input_tokens.cache_write.is_some();

    if has_granular {
        let mut cost = 0.0;
        if let Some(tokens) = usage.input_tokens.no_cache.filter(|&t| t > 0) {
            cost += tokens as f64 * pricing.input_tokens.no_cache? / PER_MILLION;
        }
        if let Some(tokens) = usage.input_tokens.cache_read.filter(|&t| t > 0) {
            cost += tokens as f64 * pricing.input_tokens.cache_read? / PER_MILLION;
        }
        if let Some(tokens) = usage.input_tokens.cache_write.filter(|&t| t > 0) {
            cost += tokens as f64 * pricing.input_tokens.cache_write? / PER_MILLION;
        }
        Some(cost)
    } else if let Some(total) = usage.input_tokens.total.filter(|&t| t > 0) {
        Some(total as f64 * pricing.input_tokens.no_cache? / PER_MILLION)
    } else {
        Some(0.0)
    }
}

fn calculate_output_cost(usage: &LanguageModelUsage, pricing: &ModelPricing) -> Option<f64> {
    let has_granular =
        usage.output_tokens.text.is_some() || usage.output_tokens.reasoning.is_some();

    if has_granular {
        let mut cost = 0.0;
        if let Some(tokens) = usage.output_tokens.text.filter(|&t| t > 0) {
            cost += tokens as f64 * pricing.output_tokens.text? / PER_MILLION;
        }
        if let Some(tokens) = usage.output_tokens.reasoning.filter(|&t| t > 0) {
            cost += tokens as f64 * pricing.output_tokens.reasoning? / PER_MILLION;
        }
        Some(cost)
    } else if let Some(total) = usage.output_tokens.total.filter(|&t| t > 0) {
        Some(total as f64 * pricing.output_tokens.text? / PER_MILLION)
    } else {
        Some(0.0)
    }
}

#[cfg(test)]
mod tests {
    use crate::models::language::usage::{
        LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage,
    };
    use crate::routers::routing_table::{InputTokenPricing, ModelPricing, OutputTokenPricing};

    use super::*;

    fn test_pricing() -> ModelPricing {
        ModelPricing {
            input_tokens: InputTokenPricing {
                no_cache: Some(2.50),
                cache_read: Some(1.25),
                cache_write: Some(3.75),
            },
            output_tokens: OutputTokenPricing {
                text: Some(10.00),
                reasoning: Some(15.00),
            },
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

        let cost = calculate_cost(&usage, &test_pricing()).expect("complete pricing");
        // input: (1000*2.50 + 1500*1.25 + 500*3.75) / 1_000_000 = 0.00625
        // output: (400*10.0 + 200*15.0) / 1_000_000 = 0.007
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

        let cost = calculate_cost(&usage, &test_pricing()).expect("complete pricing");
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

        // Pricing irrelevant when no tokens — must still report a value, not None.
        let cost = calculate_cost(&usage, &ModelPricing::default()).expect("zero usage");
        assert!((cost - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_pricing_for_nonzero_tokens_yields_none() {
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

        // Default ModelPricing has every rate = None. With nonzero tokens
        // we cannot bill — return None so callers don't silently undercharge.
        assert!(calculate_cost(&usage, &ModelPricing::default()).is_none());
    }

    #[test]
    fn partial_output_pricing_yields_none() {
        // Common production shape: input pricing complete, output pricing
        // empty (the placeholder-entry footgun the audit caught).
        let mut pricing = ModelPricing::default();
        pricing.input_tokens.no_cache = Some(2.5);
        let usage = LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(1000),
                no_cache: Some(1000),
                cache_read: None,
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(500),
                text: Some(500),
                reasoning: None,
            },
            raw: None,
        };
        assert!(calculate_cost(&usage, &pricing).is_none());
    }

    #[test]
    fn missing_rate_for_unused_bucket_is_ok() {
        // cache_read tokens are 0, so a missing cache_read rate is fine.
        let mut pricing = ModelPricing::default();
        pricing.input_tokens.no_cache = Some(2.0);
        pricing.output_tokens.text = Some(10.0);
        let usage = LanguageModelUsage {
            input_tokens: LanguageModelInputTokens {
                total: Some(1000),
                no_cache: Some(1000),
                cache_read: Some(0),
                cache_write: None,
            },
            output_tokens: LanguageModelOutputTokens {
                total: Some(500),
                text: Some(500),
                reasoning: None,
            },
            raw: None,
        };
        let cost = calculate_cost(&usage, &pricing).expect("unused bucket should not block");
        // input: 1000 * 2.0 / 1M = 0.002; output: 500 * 10.0 / 1M = 0.005
        assert!((cost - 0.007).abs() < 1e-10);
    }

    // ── FlatPricing tests ──────────────────────────────────────────────

    #[test]
    fn flat_pricing_cost_for_default() {
        let pricing = FlatPricing {
            default: 0.001,
            overrides: HashMap::new(),
        };
        assert!((pricing.cost_for("anything") - 0.001).abs() < 1e-10);
    }

    #[test]
    fn flat_pricing_cost_for_override() {
        let pricing = FlatPricing {
            default: 0.001,
            overrides: HashMap::from([("search".into(), 0.005)]),
        };
        assert!((pricing.cost_for("search") - 0.005).abs() < 1e-10);
        assert!((pricing.cost_for("other") - 0.001).abs() < 1e-10);
    }

    #[test]
    fn flat_pricing_zero_default() {
        let pricing = FlatPricing::default();
        assert_eq!(pricing.cost_for("anything"), 0.0);
    }

    #[test]
    fn flat_pricing_deserializes_with_aliases() {
        // Tool-style YAML field names
        let json = r#"{"default_cost_per_call": 0.002, "tools": {"search": 0.05}}"#;
        let parsed: FlatPricing = serde_json::from_str(json).expect("deserialize tool-style");
        assert!((parsed.default - 0.002).abs() < 1e-10);
        assert!((parsed.cost_for("search") - 0.05).abs() < 1e-10);

        // Agent-style YAML field names
        let json = r#"{"default_cost_per_call": 0.01, "methods": {"message/send": 0.1}}"#;
        let parsed: FlatPricing = serde_json::from_str(json).expect("deserialize agent-style");
        assert!((parsed.default - 0.01).abs() < 1e-10);
        assert!((parsed.cost_for("message/send") - 0.1).abs() < 1e-10);
    }
}
