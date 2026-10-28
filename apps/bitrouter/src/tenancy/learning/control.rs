use crate::{
    gateway::{
        error::{BitrouterError, Result},
        pipeline::{
            native::{
                NativeExecutionControl, NativeModelSelection, NativePlan, NativePlanAdmission,
            },
            native_work::NativeProviderWork,
        },
    },
    tenancy::learning::source::Binding,
};
use async_trait::async_trait;
use bitrouter_ai::{core::evidence::NativeAttemptReport, tenancy::identity::ExecutionId};

pub(super) struct Control {
    binding: Binding,
    execution: ExecutionId,
}
impl Control {
    pub(super) fn new(binding: Binding, execution: ExecutionId) -> Self {
        Self { binding, execution }
    }
    fn check(&self, request: &str) -> Result<()> {
        if request != self.execution.as_str() {
            return Err(BitrouterError::internal("assessment request mismatch"));
        }
        self.binding
            .assessment_check()
            .map_err(|_| BitrouterError::internal("assessment authority changed"))
    }
}
#[async_trait]
impl NativeExecutionControl for Control {
    fn model_selection(&self) -> NativeModelSelection {
        NativeModelSelection::Fixed
    }
    fn provider_response_byte_limit(&self) -> Option<u64> {
        Some(256 * 1024)
    }
    fn canonical_output_byte_limit(&self) -> Option<u64> {
        Some(256 * 1024)
    }
    async fn plan(&self, plan: NativePlan) -> Result<NativePlanAdmission> {
        self.check(&plan.request_id)?;
        if plan.routes.len() != 1 {
            return Err(BitrouterError::internal(
                "assessment requires one static candidate",
            ));
        }
        Ok(NativePlanAdmission {
            route_indices: vec![0],
        })
    }
    async fn before_attempt(&self, request: &str, index: u32) -> Result<()> {
        self.check(request)?;
        if index != 0 {
            return Err(BitrouterError::internal(
                "assessment attempt replay rejected",
            ));
        }
        Ok(())
    }
    async fn before_provider_work(&self, work: &NativeProviderWork) -> Result<()> {
        self.check(&work.request_id)
    }
    async fn after_attempt(&self, _report: NativeAttemptReport) {}
}
