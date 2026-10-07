//! Host-supplied context planning bounds and explicit cost assumptions.

use serde::{Deserialize, Serialize};

/// Bounds used by the native harness when compiling task-specific context.
/// These are local planning limits, not claims about provider capacity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DecisionPolicy {
    /// Soft prompt byte target. Required evidence may exceed it up to hard limits.
    pub target_context_bytes: usize,
    /// Most candidate evidence groups judged in one planning pass.
    pub max_candidates: usize,
    /// Maximum text preview per candidate sent to the decision model.
    pub candidate_excerpt_bytes: usize,
    /// Maximum serialized decision request, including rubrics and task text.
    pub max_request_bytes: usize,
    /// Recent complete groups always kept in the active worker's view.
    pub retain_recent_groups: usize,
    /// Minimum confidence for a lossy representation or context omission.
    pub confidence_threshold: f64,
    /// Required relative improvement before changing a feasible prior plan.
    pub minimum_savings_fraction: f64,
    /// Planning penalty for changing models; not a provider charge.
    pub model_switch_penalty_microusd: u64,
    /// Optional planning penalty per KiB of lost exact prompt prefix.
    /// Matching bytes never establish an actual provider cache hit.
    pub prefix_loss_penalty_microusd_per_kib: u64,
}

impl Default for DecisionPolicy {
    fn default() -> Self {
        Self {
            target_context_bytes: 96 * 1024,
            max_candidates: 32,
            candidate_excerpt_bytes: 2048,
            max_request_bytes: 128 * 1024,
            retain_recent_groups: 2,
            confidence_threshold: 0.85,
            minimum_savings_fraction: 0.1,
            model_switch_penalty_microusd: 1000,
            prefix_loss_penalty_microusd_per_kib: 0,
        }
    }
}

impl DecisionPolicy {
    /// Reject unusable bounds before a harness admits work.
    pub fn validate(&self) -> Result<(), String> {
        if self.target_context_bytes == 0
            || self.max_candidates == 0
            || self.candidate_excerpt_bytes == 0
            || self.max_request_bytes == 0
            || !self.confidence_threshold.is_finite()
            || !(0.5..=1.0).contains(&self.confidence_threshold)
            || !self.minimum_savings_fraction.is_finite()
            || !(0.0..1.0).contains(&self.minimum_savings_fraction)
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
