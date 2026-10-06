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
    /// Explicit generation candidates used only when a task permits model policy.
    /// Empty retains the existing model binding and context-only decisions.
    pub generation_models: Vec<GenerationModel>,
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
            generation_models: Vec::new(),
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
            || self.generation_models.len() > 16
        {
            return Err("invalid decision_model.policy bounds or confidence threshold".into());
        }
        let mut identities = std::collections::BTreeSet::new();
        for model in &self.generation_models {
            if model.model.trim().is_empty()
                || model.model.len() > 256
                || model.description.trim().is_empty()
                || model.description.len() > 4096
                || model.max_prompt_bytes == 0
                || !identities.insert(&model.model)
            {
                return Err(
                    "invalid or duplicate decision_model.policy.generation_models entry".into(),
                );
            }
        }
        Ok(())
    }
}

/// Operator-declared planning facts. Provider capacity and permission are still
/// independently checked by the SDK after selecting the concrete prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerationModel {
    /// A normal SDK route selector, never provider credentials.
    pub model: String,
    /// Capability description for the typed suitability question.
    pub description: String,
    /// Conservative local byte admission limit, not a measured token window.
    pub max_prompt_bytes: usize,
    /// Estimated micro-USD per million uncached input tokens.
    pub input_microusd_per_million: u64,
    /// Estimated micro-USD per million output tokens.
    pub output_microusd_per_million: u64,
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
