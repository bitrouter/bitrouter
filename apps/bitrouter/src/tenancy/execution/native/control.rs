//! Bound embedding callbacks inside tracked execution, including outcome cleanup.
//! Synchronous callbacks remain trusted in-process code and must not block.

use std::{future::Future, sync::Arc, time::Duration};

use super::super::TenantExecution;
use super::{ByteLimit, PROMPT_BYTES, bounded_prompt};
use crate::gateway::{
    error::Result,
    pipeline::{
        native::{
            NativeContextValidationReport, NativeExecutionControl, NativeInputCountReport,
            NativeModelSelection, NativePlan, NativePlanAdmission,
        },
        native_preparation::{NativePreparationWork, NativePreparationWorkReport},
        native_work::{NativeProviderWork, NativeProviderWorkReport},
    },
    routing::{plan::Plan, preparation::Prepared},
};
use async_trait::async_trait;
use bitrouter_ai::{
    core::evidence::{NativeAttemptReport, NativeRoute},
    tenancy::operation::ErrorCode,
    types::{Message, Prompt, StreamPart},
};

pub(super) struct Bounded {
    execution: Arc<TenantExecution>,
    inner: Arc<dyn NativeExecutionControl>,
}

impl Bounded {
    pub(super) fn new(
        execution: Arc<TenantExecution>,
        inner: Arc<dyn NativeExecutionControl>,
    ) -> Self {
        Self { execution, inner }
    }

    async fn until<T>(
        &self,
        deadline: tokio::time::Instant,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        if tokio::time::Instant::now() >= deadline {
            return Err(self.execution.reject(ErrorCode::CapacityExceeded));
        }
        tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| self.execution.reject(ErrorCode::CapacityExceeded))?
    }

    async fn admit<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        self.execution
            .revalidate()
            .map_err(|code| self.execution.reject(code))?;
        let value = self
            .until(self.execution.submission.deadline, future)
            .await?;
        self.execution
            .revalidate()
            .map_err(|code| self.execution.reject(code))?;
        Ok(value)
    }

    async fn complete<T>(&self, future: impl Future<Output = Result<T>>) -> Result<T> {
        // Revocation prevents new work; it cannot suppress accounting for work
        // already admitted. A hung outcome ACK gets at most 30 seconds, and no
        // sequence of callbacks can extend beyond request deadline + cleanup.
        let deadline = (tokio::time::Instant::now() + Duration::from_secs(30))
            .min(self.execution.submission.deadline + Duration::from_secs(30));
        self.until(deadline, future).await
    }

    fn prompt(&self, prompt: &Prompt) -> Result<()> {
        bounded_prompt(prompt).map_err(|failure| self.execution.reject(failure.0.code))
    }

    fn prepared(&self, prepared: &mut Prepared) -> Result<()> {
        if prepared.views.is_empty() || prepared.views.len() > 32 {
            return Err(self.execution.reject(ErrorCode::PayloadTooLarge));
        }
        // Charge all returned data before the shared planner validates/clones
        // views. A callback cannot give every alternative its own 16 MiB quota.
        let mut budget = ByteLimit::new();
        let measured = (|| {
            budget.value(&(
                &prepared.receipt,
                &prepared.capabilities,
                &prepared.policy,
                &prepared.previous,
            ))?;
            for view in &prepared.views {
                budget.value(&(&view.id, &view.prompt, &view.requires))?;
            }
            Ok::<_, crate::tenancy::error::Failure>(())
        })();
        measured.map_err(|failure| self.execution.reject(failure.0.code))?;
        prepared.hard_limit_bytes = prepared.hard_limit_bytes.min(PROMPT_BYTES);
        Ok(())
    }
}

#[async_trait]
impl NativeExecutionControl for Bounded {
    async fn prepare_routing(&self, prompt: &Prompt) -> Result<Option<Prepared>> {
        self.prompt(prompt)?;
        let mut prepared = self.admit(self.inner.prepare_routing(prompt)).await?;
        if let Some(prepared) = &mut prepared {
            self.prepared(prepared)?;
        }
        Ok(prepared)
    }

    async fn commit_routing(&self, plan: &Plan) -> Result<()> {
        self.prompt(&plan.prompt)?;
        self.admit(self.inner.commit_routing(plan)).await
    }

    fn observe_stream(&self) -> bool {
        self.inner.observe_stream()
    }

    async fn on_stream_part(&self, request_id: &str, part: &StreamPart) {
        let _ = self
            .until(self.execution.submission.deadline, async {
                self.inner.on_stream_part(request_id, part).await;
                Ok(())
            })
            .await;
    }

    fn provider_response_byte_limit(&self) -> Option<u64> {
        self.inner.provider_response_byte_limit()
    }
    fn canonical_output_byte_limit(&self) -> Option<u64> {
        self.inner.canonical_output_byte_limit()
    }
    fn attempt_report_byte_limit(
        &self,
        request_id: &str,
        route: &NativeRoute,
    ) -> Result<Option<u64>> {
        self.inner.attempt_report_byte_limit(request_id, route)
    }

    async fn provider_cancelled(&self) {
        tokio::select! {
            _ = self.inner.provider_cancelled() => {},
            _ = tokio::time::sleep_until(self.execution.submission.deadline) => {
                self.execution.reject(ErrorCode::CapacityExceeded);
            }
        }
    }

    async fn before_preparation_work(&self, work: &NativePreparationWork) -> Result<()> {
        self.admit(self.inner.before_preparation_work(work)).await
    }
    async fn after_preparation_work(&self, report: NativePreparationWorkReport) -> Result<()> {
        self.complete(self.inner.after_preparation_work(report))
            .await
    }
    fn auxiliary_report_byte_limit(&self, request_id: &str) -> Result<Option<u64>> {
        self.inner.auxiliary_report_byte_limit(request_id)
    }
    fn model_selection(&self) -> NativeModelSelection {
        self.inner.model_selection()
    }
    async fn before_input_count(&self, plan: &NativePlan, index: u32) -> Result<()> {
        self.prompt(&plan.prompt)?;
        self.admit(self.inner.before_input_count(plan, index)).await
    }
    async fn after_input_count(&self, report: NativeInputCountReport) -> Result<()> {
        self.complete(self.inner.after_input_count(report)).await
    }
    async fn before_context_validation(&self, request_id: &str) -> Result<()> {
        self.admit(self.inner.before_context_validation(request_id))
            .await
    }
    async fn check_context_validation(&self, request_id: &str) -> Result<()> {
        self.admit(self.inner.check_context_validation(request_id))
            .await
    }
    fn context_validation_gate_duration(&self) -> Option<Duration> {
        self.inner.context_validation_gate_duration()
    }
    async fn validate_context_rebuild(
        &self,
        original: &Prompt,
        rebuilt: &Prompt,
        request_id: &str,
    ) -> Result<()> {
        self.prompt(rebuilt)?;
        self.admit(
            self.inner
                .validate_context_rebuild(original, rebuilt, request_id),
        )
        .await
    }
    async fn after_context_validation(&self, report: NativeContextValidationReport) -> Result<()> {
        self.complete(self.inner.after_context_validation(report))
            .await
    }
    async fn plan(&self, plan: NativePlan) -> Result<NativePlanAdmission> {
        self.prompt(&plan.prompt)?;
        let count = plan.routes.len();
        let admitted = self.admit(self.inner.plan(plan)).await?;
        if admitted.route_indices.len() > count {
            return Err(self.execution.reject(ErrorCode::PayloadTooLarge));
        }
        Ok(admitted)
    }
    async fn rebuild_context(&self, rejected: &NativePlan) -> Result<Option<Vec<Message>>> {
        let result = self.admit(self.inner.rebuild_context(rejected)).await?;
        ByteLimit::new()
            .value(&result)
            .map_err(|failure| self.execution.reject(failure.0.code))?;
        Ok(result)
    }
    async fn before_attempt(&self, request_id: &str, attempt_index: u32) -> Result<()> {
        self.admit(self.inner.before_attempt(request_id, attempt_index))
            .await
    }
    async fn before_provider_work(&self, work: &NativeProviderWork) -> Result<()> {
        self.admit(self.inner.before_provider_work(work)).await
    }
    async fn after_provider_work(&self, report: NativeProviderWorkReport) {
        let _ = self
            .complete(async {
                self.inner.after_provider_work(report).await;
                Ok(())
            })
            .await;
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        let _ = self
            .complete(async {
                self.inner.after_attempt(report).await;
                Ok(())
            })
            .await;
    }
}
