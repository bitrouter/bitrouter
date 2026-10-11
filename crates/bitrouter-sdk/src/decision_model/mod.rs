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

use self::types::{DecisionError, DecisionRequest, DecisionResponse};

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
