//! Versioned paid provenance. Persisted data never constructs live authority.
use super::store::Prepared;
use crate::{
    harness::trajectory::owned::{Descriptor, io},
    tenancy::{
        learning::source::Binding,
        origin::Stamp,
        receipts::{owner::Identity, store::Candidate},
    },
};
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    identity::{CorrelationId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Seed {
    pub(super) origin: Stamp,
    pub(super) installation: InstallationId,
    pub(in crate::tenancy) tenant: TenantId,
    pub(super) principal: PrincipalId,
    pub(super) execution: ExecutionId,
    pub(super) correlation: CorrelationId,
    pub(super) configuration: RevisionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) descriptor: Option<Descriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) native: Option<super::native::Seed>,
    pub(super) session: String,
    pub(super) job: String,
    pub(super) evaluator: ResourceRef,
    pub(super) checkpoint: String,
    pub(super) prefix_digest: String,
    pub(super) input_digest: String,
}
impl Seed {
    pub(in crate::tenancy) fn from_owner(
        identity: &Identity,
        origin: Stamp,
        binding: &Binding,
        prepared: &Prepared,
    ) -> Result<Self> {
        let (installation, tenant, principal) = binding.authority().identity();
        ensure!(
            &identity.installation == installation
                && &identity.tenant == tenant
                && &identity.principal == principal
                && identity.revision == prepared.revision,
            "paid receipt identity differs from checked binding"
        );
        let (descriptor, connection, native) = match (binding, &prepared.source) {
            (Binding::Capture(binding), super::store::PreparedSource::Capture(_)) => (
                Some(binding.association().descriptor().clone()),
                Some(binding.association().connection().to_owned()),
                None,
            ),
            (Binding::Native(binding), super::store::PreparedSource::Native(source)) => (
                None,
                None,
                Some(super::native::Seed::checked(binding, source)?),
            ),
            _ => anyhow::bail!("paid source and owner profile mismatch"),
        };
        let value = Self {
            origin,
            installation: identity.installation.clone(),
            tenant: identity.tenant.clone(),
            principal: identity.principal.clone(),
            execution: identity.execution.clone(),
            correlation: identity.correlation.clone(),
            configuration: identity.revision.clone(),
            descriptor,
            connection,
            native,
            session: prepared.session.clone(),
            job: prepared.id.clone(),
            evaluator: prepared.route.clone(),
            checkpoint: prepared.checkpoint().to_owned(),
            prefix_digest: prepared.input.evidence.prefix_digest.clone(),
            input_digest: prepared.input_digest.clone(),
        };
        value.validate()?;
        Ok(value)
    }
    pub(super) fn validate(&self) -> Result<()> {
        self.origin.validate()?;
        match (&self.descriptor, &self.connection, &self.native) {
            (Some(descriptor), Some(connection), None) => {
                ensure!(
                    descriptor.version == 1
                        && descriptor.installation == self.installation.as_str()
                        && descriptor.tenant == self.tenant.as_str()
                        && descriptor.principal == self.principal.as_str()
                        && descriptor.revision == self.configuration.as_str()
                        && !connection.is_empty()
                        && connection.len() <= 256
                        && !connection.chars().any(char::is_control),
                    "paid capture seed mismatch"
                );
            }
            (None, None, Some(native)) => native.validate(self)?,
            _ => anyhow::bail!("paid source kind is ambiguous"),
        }
        for value in [&self.session, &self.job, &self.checkpoint] {
            ensure!(
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
                "paid seed identifier bound"
            );
        }
        let prefix = if self.native.is_some() {
            self.prefix_digest
                .strip_prefix("sha256:")
                .ok_or_else(|| anyhow::anyhow!("native paid prefix domain"))?
        } else {
            self.prefix_digest.as_str()
        };
        for digest in [prefix, self.input_digest.as_str()] {
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "paid seed digest"
            );
        }
        io::json_len(self, 16 * 1024)?;
        Ok(())
    }
    pub(super) fn matches(&self, p: &Prepared, execution: &ExecutionId) -> bool {
        self.execution == *execution
            && self.configuration == p.revision
            && self.session == p.session
            && self.job == p.id
            && self.evaluator == p.route
            && self.checkpoint == *p.checkpoint()
            && self.prefix_digest == p.input.evidence.prefix_digest
            && self.input_digest == p.input_digest
    }
    pub(super) fn receipt(&self, candidate: &Candidate) -> Result<()> {
        let receipt = &candidate.receipt;
        ensure!(
            receipt.installation_id == self.installation
                && receipt.tenant_id == self.tenant
                && receipt.principal_id == self.principal
                && receipt.execution_id == self.execution
                && receipt.correlation_id == self.correlation
                && receipt.execution_revision == self.configuration,
            "paid observed receipt identity mismatch"
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Observed {
    pub receipt_digest: String,
    pub output_digest: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retained {
    pub seed: Seed,
    pub observed: Option<Observed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<Stopped>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_cleanup: Option<super::native::Cleanup>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Member {
    pub tenant: TenantId,
    pub execution: ExecutionId,
    pub job: String,
    pub seed_digest: String,
}
impl Member {
    pub(super) fn from_seed(seed: &Seed) -> Result<Self> {
        seed.validate()?;
        Ok(Self {
            tenant: seed.tenant.clone(),
            execution: seed.execution.clone(),
            job: seed.job.clone(),
            seed_digest: crate::storage::evolution::digest(seed)?,
        })
    }
}
/// Exact old Registry membership; data readback grants no live capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Stopped {
    pub members: Vec<Member>,
    pub set_digest: String,
    pub local_cleanup_digest: String,
}
impl Stopped {
    pub(super) fn validate(&self, seed: &Seed) -> Result<()> {
        ensure!(
            !self.members.is_empty() && self.members.len() <= 8,
            "paid stop set bound"
        );
        let own = Member::from_seed(seed)?;
        let mut seen = std::collections::BTreeSet::new();
        let mut counts = std::collections::BTreeMap::new();
        for member in &self.members {
            ensure!(
                seen.insert(member.execution.as_str()),
                "duplicate stopped execution"
            );
            let count = counts.entry(member.tenant.as_str()).or_insert(0usize);
            *count += 1;
            ensure!(*count <= 2, "stopped tenant capacity");
            ensure!(
                member.job.len() <= 256
                    && !member.job.is_empty()
                    && lower_digest(&member.seed_digest),
                "stopped member identity"
            );
        }
        ensure!(
            self.members.contains(&own)
                && self
                    .members
                    .windows(2)
                    .all(|pair| pair[0].execution.as_str() < pair[1].execution.as_str())
                && crate::storage::evolution::digest(&(&seed.origin, &self.members))?
                    == self.set_digest
                && lower_digest(&self.local_cleanup_digest),
            "paid stop set digest mismatch"
        );
        Ok(())
    }
}
fn lower_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Data namespace for a bounded bootstrap participant, never execution scope.
/// Construction requires the same actually claimed pool/H owner before ready.
pub(crate) struct RecoveryIdentity {
    database: sea_orm::DatabaseConnection,
    installation: InstallationId,
    tenant: TenantId,
    principal: PrincipalId,
}
impl RecoveryIdentity {
    pub(super) fn checked(
        database: &sea_orm::DatabaseConnection,
        owner: &crate::tenancy::origin::StartupOwner,
        proof: &std::sync::Arc<crate::tenancy::transaction::Proof>,
        installation: &InstallationId,
        tenant: &TenantId,
        principal: PrincipalId,
    ) -> Result<Self> {
        owner.bootstrap_stamp(proof)?;
        Ok(Self {
            database: database.clone(),
            installation: installation.clone(),
            tenant: tenant.clone(),
            principal: principal.clone(),
        })
    }
    pub(crate) fn database(&self) -> &sea_orm::DatabaseConnection {
        &self.database
    }
    pub(crate) fn storage_identity(&self) -> (&InstallationId, &TenantId, &PrincipalId) {
        (&self.installation, &self.tenant, &self.principal)
    }
}

impl Seed {
    pub(super) fn unknown(
        &self,
        at: &bitrouter_ai::tenancy::identity::Timestamp,
    ) -> Result<Candidate> {
        use bitrouter_ai::tenancy::{
            execution::{ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
            operation::DispatchState,
        };
        Candidate::new(
            ExecutionReceipt {
                installation_id: self.installation.clone(),
                tenant_id: self.tenant.clone(),
                principal_id: self.principal.clone(),
                execution_id: self.execution.clone(),
                correlation_id: self.correlation.clone(),
                execution_revision: self.configuration.clone(),
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
            crate::tenancy::receipts::store::Disposition {
                dispatch: DispatchState::Unknown,
                statuses: Vec::new(),
                auxiliary_digest: None,
            },
        )
    }
}
