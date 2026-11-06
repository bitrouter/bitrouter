//! One explicitly retained OAuth lineage. No ambient accounts or new lifecycle.
pub(in crate::tenancy) mod adapter;
pub(in crate::tenancy) mod crypto;
pub(in crate::tenancy) mod exchange;
pub(in crate::tenancy) mod profile;
pub(in crate::tenancy) mod refresh;
pub(in crate::tenancy) mod storage;

use storage::Metadata;

/// Minted by the actual selected adapter; exact request binding stays private.
#[derive(Clone)]
pub(crate) struct ModelStamp {
    pub(in crate::tenancy) metadata: Metadata,
    pub(in crate::tenancy) execution: bitrouter_ai::tenancy::identity::ExecutionId,
}
impl ModelStamp {
    pub(in crate::tenancy) fn matches(
        &self,
        execution: &super::execution::TenantExecution,
        metadata: &Metadata,
    ) -> bool {
        &self.execution == execution.id()
            && self.metadata == *metadata
            && matches!(metadata.state, storage::State::Ready)
    }
}
