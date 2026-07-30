//! Evaluator-agnostic evidence exchange and policy compilation control plane.

pub mod admission;
pub mod settlement;
pub mod store;
pub mod types;

use bitrouter_sdk::config::EvalConfig;

use self::admission::{AdmissionOutcome, SubmissionPrincipal};
use self::store::EvalStore;
use self::types::EvaluationResult;

#[derive(Clone)]
pub struct EvalService {
    store: EvalStore,
    config: EvalConfig,
}

impl EvalService {
    pub fn new(store: EvalStore, config: EvalConfig) -> Self {
        Self { store, config }
    }

    pub fn store(&self) -> &EvalStore {
        &self.store
    }

    pub async fn submit(
        &self,
        result: EvaluationResult,
        principal: SubmissionPrincipal,
    ) -> anyhow::Result<AdmissionOutcome> {
        admission::submit(&self.store, &self.config, result, principal).await
    }
}
