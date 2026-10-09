//! Frozen semantic evidence and context alternatives supplied to one router.

use super::{
    ContextCapability,
    assessment::{self, Assessment},
    input::Input,
    plan::View,
};
use crate::decision_model::{
    DecisionRuntime,
    policy::DecisionPolicy,
    types::{DecisionError, DecisionFailure, DecisionUsage},
};
use crate::language_model::native::NativePlan;
use bitrouter_ai::types::Prompt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// One semantic attempt, independent of the subsequent generation outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    /// Stable identity used to deduplicate reuse across executions.
    pub id: String,
    /// Accepted labels and original distributions, when available.
    pub assessment: Option<Assessment>,
    /// Failed or unavailable semantic evidence never grants context authority.
    pub error: Option<DecisionError>,
    /// Observed usage; absence is unknown, never zero.
    pub usage: Option<DecisionUsage>,
}

impl crate::event::PipelineEvent for Receipt {
    fn event_name(&self) -> &'static str {
        "routing.assessed"
    }
}

/// Trusted host preparation. Transport metadata cannot construct this value.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// Shared semantic evidence, possibly from an acknowledged cached attempt.
    pub receipt: Option<Receipt>,
    /// Finite context alternatives, all derived from the admitted source.
    pub views: Vec<View>,
    /// Explicit rights granted by the owner of the source evidence.
    pub capabilities: BTreeSet<ContextCapability>,
    /// Frozen local bounds and deterministic cost assumptions.
    pub policy: DecisionPolicy,
    /// Previous execution, if known; source type is irrelevant.
    pub previous: Option<NativePlan>,
    /// Maximum canonical generation input size admitted by the host.
    pub hard_limit_bytes: usize,
}

impl Prepared {
    /// Context candidates may change evidence messages only. Model and effort
    /// belong to the policy stage; tools, instructions and request controls stay
    /// bound to the checked source envelope.
    pub(crate) fn validate_for(&self, original: &Prompt) -> crate::Result<()> {
        self.policy
            .validate()
            .map_err(crate::BitrouterError::bad_request)?;
        let original_value = serde_json::to_value(original)
            .map_err(|error| crate::BitrouterError::bad_request(error.to_string()))?;
        for view in &self.views {
            let mut envelope = view.prompt.clone();
            envelope.messages = original.messages.clone();
            envelope.model = original.model.clone();
            if serde_json::to_value(&envelope)
                .map_err(|error| crate::BitrouterError::bad_request(error.to_string()))?
                != original_value
            {
                return Err(crate::BitrouterError::bad_request(
                    "context candidate changed the admitted request envelope",
                ));
            }
        }
        Ok(())
    }

    /// Conservative preparation for a caller-owned prompt. Rich public history
    /// is classified normally; context rights must be supplied by its owner.
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
            views: vec![View {
                id: "full".into(),
                prompt: prompt.clone(),
                requires: BTreeSet::new(),
            }],
            capabilities: BTreeSet::new(),
            policy,
            previous: None,
            hard_limit_bytes: usize::MAX,
        }
    }
}
