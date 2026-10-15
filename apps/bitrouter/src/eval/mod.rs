//! Evaluator-agnostic evidence exchange and policy compilation control plane.

pub mod admission;
pub mod api;
pub mod compiler;
pub mod settlement;
pub mod store;

use crate::config::EvalConfig;

use self::store::EvalStore;
use bitrouter_ai::eval::EvaluationResult;
use {self::admission::SubmissionPrincipal, bitrouter_ai::eval::AdmissionOutcome};

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
