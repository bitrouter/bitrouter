//! Real admission provenance for conservative predecessor terminal repair.
use super::store::{Candidate, Disposition};
use crate::tenancy::{execution::session::Descriptor, origin::Stamp};
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
    identity::{
        CorrelationId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId, Timestamp,
    },
    operation::DispatchState,
};
use serde::{Deserialize, Serialize};

/// Immutable facts copied only from an actual receipt Owner. Deserialization is
/// evidence validation, never admission or an authority constructor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Seed {
    pub(in crate::tenancy) installation: InstallationId,
    pub(in crate::tenancy) tenant: TenantId,
    pub(in crate::tenancy) principal: PrincipalId,
    pub(in crate::tenancy) execution: ExecutionId,
    correlation: CorrelationId,
    revision: RevisionId,
    pub(in crate::tenancy) origin: Stamp,
}
impl Seed {
    pub(crate) fn execution_id(&self) -> &ExecutionId {
        &self.execution
    }

    pub(super) fn from_owner(identity: &super::owner::Identity, origin: Stamp) -> Self {
        Self {
            installation: identity.installation.clone(),
            tenant: identity.tenant.clone(),
            principal: identity.principal.clone(),
            execution: identity.execution.clone(),
            correlation: identity.correlation.clone(),
            revision: identity.revision.clone(),
            origin,
        }
    }
    pub(crate) fn matches(&self, descriptor: &Descriptor, execution: &ExecutionId) -> bool {
        self.installation == descriptor.installation
            && self.tenant == descriptor.tenant
            && self.principal == descriptor.principal
            && self.revision == descriptor.revision
            && &self.execution == execution
    }
    pub(in crate::tenancy) fn check(&self, descriptor: &Descriptor, origin: &Stamp) -> Result<()> {
        self.origin.validate()?;
        ensure!(
            self.matches(descriptor, &self.execution) && &self.origin == origin,
            "receipt seed provenance mismatch"
        );
        Ok(())
    }
    pub(in crate::tenancy) fn validate_receipt(&self, value: &ExecutionReceipt) -> Result<()> {
        ensure!(
            value.installation_id == self.installation
                && value.tenant_id == self.tenant
                && value.principal_id == self.principal
                && value.execution_id == self.execution
                && value.correlation_id == self.correlation
                && value.execution_revision == self.revision,
            "receipt seed identity mismatch"
        );
        Ok(())
    }
    pub(in crate::tenancy) fn unknown(&self, at: &Timestamp) -> Result<Candidate> {
        Candidate::new(
            ExecutionReceipt {
                installation_id: self.installation.clone(),
                tenant_id: self.tenant.clone(),
                principal_id: self.principal.clone(),
                execution_id: self.execution.clone(),
                correlation_id: self.correlation.clone(),
                execution_revision: self.revision.clone(),
                state: ReceiptState::Unknown,
                outcome: Outcome::Unknown,
                attempts: Vec::new(),
                usage: Usage {
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                    provenance: Provenance::Unknown,
                    complete: false,
                },
                recorded_at: at.clone(),
            },
            Disposition {
                dispatch: DispatchState::Unknown,
                statuses: Vec::new(),
                auxiliary_digest: None,
            },
        )
    }
}
