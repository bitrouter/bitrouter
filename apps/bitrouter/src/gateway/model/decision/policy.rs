//! Host-supplied bounds for semantic model classification.

use serde::{Deserialize, Serialize};

/// Request size and confidence limits for the semantic classifier.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DecisionPolicy {
    /// Maximum serialized decision request, including rubrics and public text.
    pub max_request_bytes: usize,
    /// Minimum confidence required to accept a classification.
    pub confidence_threshold: f64,
}

impl Default for DecisionPolicy {
    fn default() -> Self {
        Self {
            max_request_bytes: 128 * 1024,
            confidence_threshold: 0.85,
        }
    }
}

impl DecisionPolicy {
    /// Reject unusable bounds before admitting semantic work.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_request_bytes == 0
            || !self.confidence_threshold.is_finite()
            || !(0.5..=1.0).contains(&self.confidence_threshold)
        {
            return Err("invalid decision_model.policy bounds or confidence threshold".into());
        }
        Ok(())
    }
}

/// Optional token prices supplied by the operator, distinct from provider bills.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DecisionPricing {
    /// USD per million input tokens.
    pub input_usd_per_million: f64,
    /// USD per million output tokens.
    pub output_usd_per_million: f64,
}

impl DecisionPricing {
    /// Reject non-finite and negative amounts.
    pub fn validate(&self) -> Result<(), String> {
        if [self.input_usd_per_million, self.output_usd_per_million]
            .into_iter()
            .any(|amount| !amount.is_finite() || amount < 0.0)
        {
            return Err("decision_model.pricing must contain finite nonnegative prices".into());
        }
        Ok(())
    }

    /// Estimate tokens at the frozen prices, without claiming a settled charge.
    pub fn estimate(&self, usage: super::types::DecisionUsage) -> f64 {
        (usage.input_tokens as f64 * self.input_usd_per_million
            + usage.output_tokens as f64 * self.output_usd_per_million)
            / 1_000_000.0
    }
}
