//! Typed semantic decisions, separate from language generation and request checks.
//!
//! The caller owns durable intent/result commits and authority checks. Executors
//! never retry: a lost response may already have been billed by the provider.

pub mod policy;
pub mod types;
pub mod typesafe;

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use self::types::{DecisionError, DecisionFailure, DecisionRequest, DecisionResponse};
use crate::language_model::native::NativeExecutionControl;

/// One cancellable decision attempt. A validated response retains provider usage.
#[async_trait]
pub trait DecisionExecutor: Send + Sync {
    /// Execute once. Implementations must validate answers against the request.
    async fn execute(
        &self,
        request: &DecisionRequest,
        cancellation: &CancellationToken,
    ) -> Result<DecisionResponse, DecisionError>;
}

/// Host-injected decision backend and its explicit model binding.
#[derive(Clone)]
pub struct DecisionRuntime {
    /// Provider model name, independent of language-model route selectors.
    pub model: String,
    /// Single-attempt executor; durable scheduling belongs to the harness.
    pub executor: Arc<dyn DecisionExecutor>,
    /// Host bounds for evidence selection, frozen by each admitted intent.
    pub policy: policy::DecisionPolicy,
    /// Optional declared prices used only for token estimates.
    pub pricing: Option<policy::DecisionPricing>,
}

impl DecisionRuntime {
    /// Execute one typed request through the host's admission and outcome barriers.
    /// Host failures abort the call; provider failures remain recorded outcomes.
    pub async fn execute_controlled(
        &self,
        attempt_id: &str,
        request: &DecisionRequest,
        control: &dyn NativeExecutionControl,
    ) -> crate::Result<Result<DecisionResponse, DecisionError>> {
        let admission = control
            .before_typed_decision(attempt_id, request, self.pricing.as_ref())
            .await;
        let token = CancellationToken::new();
        let outcome = match &admission {
            Err(error) => Err(DecisionError::invalid_request(error.to_string())),
            Ok(()) => {
                let call = self.execute_bounded(request, &token);
                tokio::pin!(call);
                tokio::select! {
                    biased;
                    () = control.provider_cancelled() => {
                        token.cancel();
                        // Retain usage already observed instead of dropping the attempt.
                        call.await
                    },
                    outcome = &mut call => outcome,
                }
            }
        };
        control
            .after_typed_decision(attempt_id, &outcome, self.pricing.as_ref())
            .await?;
        admission?;
        Ok(outcome)
    }

    pub(crate) async fn execute_bounded(
        &self,
        request: &DecisionRequest,
        token: &CancellationToken,
    ) -> Result<DecisionResponse, DecisionError> {
        let policy = &self.policy;
        let outcome = match serde_json::to_vec(request) {
            Ok(bytes)
                if bytes.len() <= policy.max_request_bytes
                    && policy.validate().is_ok()
                    && request.validate().is_ok() =>
            {
                self.executor.execute(request, token).await
            }
            _ => Err(DecisionError::invalid_request(
                "typed decision request exceeds its admitted bound",
            )),
        };
        let outcome = outcome.and_then(|response| {
            response.validate(request)?;
            Ok(response)
        });
        let limit = policy
            .max_request_bytes
            .saturating_mul(2)
            .saturating_add(8192);
        if serde_json::to_vec(&outcome).is_ok_and(|bytes| bytes.len() <= limit) {
            return outcome;
        }
        let usage = match &outcome {
            Ok(response) => Some(response.usage),
            Err(error) => error.usage,
        };
        let may_have_run = match &outcome {
            Ok(_) => true,
            Err(error) => error.may_have_run,
        };
        Err(DecisionError {
            kind: DecisionFailure::ResponseTooLarge,
            message: "typed decision outcome exceeds its admitted bound".into(),
            usage,
            may_have_run,
        })
    }
}
