//! Bounded semantic evidence supplied to model selection.

use super::{
    assessment::{self, Assessment},
    input::Input,
};
use crate::gateway::model::decision::DecisionRuntime;
use crate::gateway::model::decision::policy::DecisionPolicy;
use crate::gateway::model::decision::types::DecisionError;
use crate::gateway::model::decision::types::DecisionFailure;
use crate::gateway::model::decision::types::DecisionUsage;
use bitrouter_ai::types::Prompt;
use serde::{Deserialize, Serialize};

/// One semantic attempt, independent of the subsequent generation outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    /// Stable identity used to deduplicate reuse across executions.
    pub id: String,
    /// Accepted labels and original distributions, when available.
    pub assessment: Option<Assessment>,
    /// Failed or unavailable semantic evidence causes model-selection abstention.
    pub error: Option<DecisionError>,
    /// Observed usage; absence is unknown, never zero.
    pub usage: Option<DecisionUsage>,
}

impl crate::gateway::observability::event::PipelineEvent for Receipt {
    fn event_name(&self) -> &'static str {
        "routing.assessed"
    }
}

/// Trusted host preparation. Transport metadata cannot construct this value.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// Shared semantic evidence, possibly from an acknowledged cached attempt.
    pub receipt: Option<Receipt>,
    /// Frozen classifier bounds and confidence threshold.
    pub policy: DecisionPolicy,
    /// Maximum canonical generation input size admitted by the host.
    pub hard_limit_bytes: usize,
}

impl Prepared {
    /// Validate the classifier policy and the unchanged generation input bound.
    pub(crate) fn validate_for(&self, prompt: &Prompt) -> crate::gateway::error::Result<()> {
        use crate::gateway::error::BitrouterError;
        self.policy
            .validate()
            .map_err(BitrouterError::bad_request)?;
        let size = serde_json::to_vec(prompt)
            .map_err(|error| BitrouterError::bad_request(error.to_string()))?
            .len();
        if size > self.hard_limit_bytes {
            return Err(BitrouterError::bad_request(
                "model input exceeds its admitted byte bound",
            ));
        }
        Ok(())
    }

    /// Classify bounded public history without changing the caller's prompt.
    pub async fn from_prompt(prompt: &Prompt, runtime: Option<&DecisionRuntime>, id: &str) -> Self {
        let policy = runtime
            .map(|runtime| runtime.policy.clone())
            .unwrap_or_default();
        let receipt = if let Some(runtime) = runtime {
            let input = Input::from_prompt(prompt, policy.max_request_bytes / 4);
            let request = assessment::request(&runtime.model, &input);
            let token = tokio_util::sync::CancellationToken::new();
            let outcome = match serde_json::to_vec(&request) {
                Ok(bytes)
                    if bytes.len() <= policy.max_request_bytes
                        && policy.validate().is_ok()
                        && request.validate().is_ok() =>
                {
                    runtime.executor.execute(&request, &token).await
                }
                _ => Err(DecisionError::invalid_request(
                    "routing classifier request exceeds its admitted bound",
                )),
            };
            let response_limit = policy
                .max_request_bytes
                .saturating_mul(2)
                .saturating_add(8192);
            let outcome = match outcome {
                Ok(response)
                    if serde_json::to_vec(&response)
                        .is_ok_and(|bytes| bytes.len() <= response_limit) =>
                {
                    Ok(response)
                }
                Ok(response) => Err(DecisionError {
                    kind: DecisionFailure::ResponseTooLarge,
                    message: "routing classifier response exceeds its admitted bound".into(),
                    usage: Some(response.usage),
                    may_have_run: true,
                }),
                Err(error) => Err(error),
            };
            let usage = match &outcome {
                Ok(response) => Some(response.usage),
                Err(error) => error.usage,
            };
            let decoded = outcome.and_then(|response| {
                assessment::decode(&request, &response, policy.confidence_threshold)
            });
            let (assessment, error) = match decoded {
                Ok(value) => (Some(value), None),
                Err(error) => (None, Some(error)),
            };
            Some(Receipt {
                id: id.into(),
                assessment,
                error,
                usage,
            })
        } else {
            None
        };
        Self {
            receipt,
            policy,
            hard_limit_bytes: usize::MAX,
        }
    }
}
