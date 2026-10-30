//! Native run assignments use H, the existing journal, receipts and trajectory.
pub(crate) mod queue;
pub(in crate::tenancy) mod store;

use crate::{
    gateway::routing::exploration::RouteExploration,
    harness::resources::managed::store::model::Origin, tenancy::execution::TenantExecution,
};
use bitrouter_ai::{
    eval::EvalExperimentRef,
    tenancy::{
        configuration::ResourceRef,
        identity::{ExecutionId, RevisionId},
    },
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Profile {
    pub(in crate::tenancy) revision: RevisionId,
    pub(in crate::tenancy) route: ResourceRef,
    pub(in crate::tenancy) policy: ResourceRef,
    pub(in crate::tenancy) policy_name: String,
    pub(in crate::tenancy) policy_digest: String,
    pub(in crate::tenancy) definition_digest: String,
    pub(in crate::tenancy) treatment: RouteExploration,
}

/// Only an acknowledged H enrollment produces this runtime routing input.
/// It contains no execution lease and cannot be reconstructed from stored labels.
pub(crate) struct Witness {
    execution: ExecutionId,
    unit: store::Unit,
    origin: Arc<Origin>,
}
impl Witness {
    pub(in crate::tenancy) fn matches(&self, execution: &TenantExecution) -> bool {
        self.execution == *execution.id()
            && execution.matches_experiment(&self.unit.profile, &self.origin)
            && self.unit.facts_match(self.origin.facts())
    }
    pub(crate) fn assignment(&self) -> &EvalExperimentRef {
        &self.unit.assignment
    }
    pub(in crate::tenancy) fn episode(&self) -> &str {
        &self.unit.episode
    }
}

impl Witness {
    pub(crate) fn assignment_for(
        &self,
        treatment: &RouteExploration,
    ) -> crate::gateway::error::Result<&EvalExperimentRef> {
        if treatment != &self.unit.profile.treatment {
            return Err(crate::gateway::error::BitrouterError::Forbidden(
                "native experiment treatment changed".into(),
            ));
        }
        Ok(&self.unit.assignment)
    }
}

/// An exact persisted unit/step edge, usable only in its writer transaction.
pub(crate) struct BoundEpisode {
    episode: String,
    selected_tier: Option<String>,
}
impl BoundEpisode {
    pub(crate) fn episode(&self) -> &str {
        &self.episode
    }
    pub(crate) fn check_selected(&self, selected: Option<&str>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.selected_tier
                .as_deref()
                .is_none_or(|expected| selected == Some(expected)),
            crate::storage::operation_budget::Error::Rejected(
                bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch
            )
        );
        Ok(())
    }
}

/// Read-only evidence addresses produced by validated experiment records.
pub(crate) struct PublicationRef<'a> {
    pub(crate) request: &'a str,
    pub(crate) event: &'a str,
    pub(crate) outbox: &'a str,
}

/// The original native write and its ACK participant share this fixed envelope.
/// Five full representations cover input/parse/write plus ACK decode/readback.
/// Every one of the 128 pending steps may read its unit twice and replace both
/// unit and step, in addition to the pending scan: five bounded record transfers.
pub(crate) fn check_native_allowance(
    bytes: u64,
) -> Result<(), crate::storage::operation_budget::Error> {
    use crate::storage::operation_budget::{Error, OperationBudget};
    let bytes = usize::try_from(bytes).map_err(|_| Error::Capacity)?;
    let work = bytes
        .checked_mul(5)
        .and_then(|n| n.checked_add(128 * 5 * (8192 + 4096)))
        .and_then(|n| n.checked_add(32 * 1024))
        .ok_or(Error::Capacity)?;
    let check = || Ok(());
    OperationBudget::new(&check).charge(128 * 5 + 8, work)
}
