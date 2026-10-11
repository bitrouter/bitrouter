//! Host configuration for a typed decision backend. Credentials are read by
//! the host, never serialized into runtime decisions or checkpoint state.

use serde::{Deserialize, Serialize};

use crate::decision_model::policy::{DecisionPolicy, DecisionPricing};
use crate::error::{BitrouterError, Result};

/// An optional `decision_model:` block enables native context decisions.
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
    /// Native context planning limits, frozen into each decision intent.
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
