//! Host configuration for a typed decision backend. Credentials are read by
//! the host, never serialized into runtime decisions or checkpoint state.

use serde::{Deserialize, Serialize};

use crate::gateway::error::BitrouterError;
use crate::gateway::error::Result;

/// An optional `decision_model:` block enables SDK routing assessment.
/// Native orchestrator context and skill selection do not invoke this backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DecisionModelConfig {
    /// Decision model name or alias from TypeSafe's authenticated model list.
    pub model: String,
    /// TypeSafe API root; a compatible local endpoint can be used for testing.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Environment variable containing the API credential.
    #[serde(default = "default_key_env")]
    pub api_key_env: String,
    /// Whole-attempt timeout, including reading and validating the response.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum response bytes; independent of context and checkpoint limits.
    #[serde(default = "default_response_bytes")]
    pub max_response_bytes: usize,
    /// Semantic classification bounds, frozen into each request.
    #[serde(default)]
    pub policy: DecisionPolicy,
    /// Optional operator-supplied token prices; estimates, never settled bills.
    #[serde(default)]
    pub pricing: Option<DecisionPricing>,
}

impl DecisionModelConfig {
    /// Validate without looking up credentials or making network requests.
    pub fn validate(&self) -> Result<()> {
        if self.model.trim().is_empty()
            || self.api_key_env.trim().is_empty()
            || self.timeout_ms == 0
            || self.max_response_bytes == 0
        {
            return Err(BitrouterError::bad_request(
                "invalid decision_model connection settings",
            ));
        }
        let endpoint = url::Url::parse(&self.base_url)
            .map_err(|_| BitrouterError::bad_request("invalid decision_model.base_url"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(BitrouterError::bad_request(
                "invalid decision_model.base_url",
            ));
        }
        self.policy
            .validate()
            .map_err(BitrouterError::bad_request)?;
        if let Some(pricing) = &self.pricing {
            pricing.validate().map_err(BitrouterError::bad_request)?;
        }
        Ok(())
    }
}

fn default_base_url() -> String {
    "https://api.typesafe.ai".into()
}
fn default_key_env() -> String {
    "TYPESAFE_API_KEY".into()
}
fn default_timeout_ms() -> u64 {
    30_000
}
fn default_response_bytes() -> usize {
    1_048_576
}

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
    ///
    /// # Errors
    /// Returns an error for a zero request bound or confidence outside 0.5–1.0.
    pub fn validate(&self) -> std::result::Result<(), String> {
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
    ///
    /// # Errors
    /// Returns an error if either configured price is negative or non-finite.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if [self.input_usd_per_million, self.output_usd_per_million]
            .into_iter()
            .any(|amount| !amount.is_finite() || amount < 0.0)
        {
            return Err("decision_model.pricing must contain finite nonnegative prices".into());
        }
        Ok(())
    }
}
