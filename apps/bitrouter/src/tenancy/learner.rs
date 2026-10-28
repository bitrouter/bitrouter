//! Explicit owned learning registrations use the existing H actor and worker.
pub(crate) mod identity;
pub(in crate::tenancy) mod queue;
pub(in crate::tenancy) mod registration;
pub(in crate::tenancy) mod source;
pub(in crate::tenancy) mod store;
#[cfg(all(test, unix))]
pub(in crate::tenancy) mod testing;
pub(crate) mod worker;

use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    identity::{AuthorityRevision, InstallationId, OperationId, RevisionId, TenantId},
};
use serde::{Deserialize, Serialize};

pub(in crate::tenancy) const MAX_DUE: usize = 32;
pub(in crate::tenancy) const MAX_ROWS: usize = 4096;
pub(in crate::tenancy) const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Private operator/local request; no public HTTP endpoint deserializes it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct RegistrationInput {
    pub installation: InstallationId,
    pub tenant: TenantId,
    pub id: String,
    pub operation_id: OperationId,
    pub expected_authority_revision: AuthorityRevision,
    pub expected_active_revision: RevisionId,
    pub expected_schedule_revision: Option<u64>,
    pub route: ResourceRef,
    pub evidence_root: String,
    pub enabled: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Registration {
    pub version: u32,
    pub input: RegistrationInput,
    pub revision: u64,
    pub creator: bitrouter_ai::tenancy::identity::PrincipalId,
    pub ceiling: identity::Ceiling,
    pub profile: crate::tenancy::experiment::Profile,
    pub settings_digest: String,
}
