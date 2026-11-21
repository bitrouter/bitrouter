//! Explicit provider/model limits and capability declarations.

use bitrouter_ai::types::ReasoningEffort;
use serde::{Deserialize, Serialize};

/// Independently declared token limits. An input limit is not a combined
/// context window, and an absent limit is unknown rather than unbounded.
#[derive(schemars::JsonSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelTokenLimits {
    /// Maximum input tokens, independently of the output allowance.
    pub max_input_tokens: Option<u64>,
    /// Maximum generated tokens for one request.
    pub max_output_tokens: Option<u64>,
    /// Combined input and output capacity, only when explicitly known.
    pub context_window: Option<u64>,
}

/// Explicit provider support, never inferred from a compatible generation API.
#[derive(schemars::JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTokenCounting {
    /// POST /responses/input_tokens on the same provider endpoint.
    Responses,
}

/// Exact qualitative reasoning-effort support for one provider/model route.
///
/// Absence means unknown rather than unsupported. Policy-owned effort targets
/// require this positive declaration; ordinary caller-authored parameters keep
/// their existing pass-through behavior when support is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReasoningEffortConfig {
    /// Effort values accepted by this exact provider/model route.
    pub levels: Vec<ReasoningEffort>,
    /// Provider default when the request omits an effort value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<ReasoningEffort>,
}

impl ReasoningEffortConfig {
    /// Validate the declared reasoning support.
    ///
    /// # Errors
    /// Rejects empty or duplicate levels and a default outside the declared levels.
    pub fn validate(&self) -> Result<(), String> {
        if self.levels.is_empty() {
            return Err("invalid model request: reasoning_effort.levels must not be empty".into());
        }
        let mut unique = std::collections::BTreeSet::new();
        for level in &self.levels {
            if !unique.insert(*level) {
                return Err(format!(
                    "invalid model request: reasoning_effort.levels contains duplicate '{level}'"
                ));
            }
        }
        if let Some(default) = self.default
            && !self.levels.contains(&default)
        {
            return Err(format!(
                "invalid model request: reasoning_effort.default '{default}' must be listed in levels"
            ));
        }
        Ok(())
    }
}
