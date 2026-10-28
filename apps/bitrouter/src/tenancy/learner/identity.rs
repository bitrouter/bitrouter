//! Persisted ceilings are provenance. Only a fresh H check grants a read.
use super::Registration;
use crate::tenancy::{
    authority::{covers_key, scope_subset},
    model::{Caller, Snapshot},
    transaction::Proof,
};
use bitrouter_ai::tenancy::{
    authority::{GrantScope, Permission},
    configuration::ResourceRef,
    identity::{
        BindingId, CredentialId, InstallationId, PrincipalId, RevisionId, TenantId, Timestamp,
    },
    operation::ErrorCode,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::time::Instant;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Ceiling {
    credential: CredentialId,
    credential_revision: RevisionId,
    principal: PrincipalId,
    binding: BindingId,
    credential_scope: GrantScope,
    binding_scope: GrantScope,
    permissions: Vec<Permission>,
    credential_expires: Timestamp,
    binding_expires: Timestamp,
}

fn tenant_scope() -> GrantScope {
    GrantScope::Tenant {
        operator_pools: Vec::new(),
    }
}

impl Ceiling {
    /// Called with H's authenticated caller and current immutable authority view.
    pub(in crate::tenancy) fn capture(
        state: &Snapshot,
        caller: &Caller,
        tenant: &TenantId,
    ) -> Result<Self, ErrorCode> {
        let current = state.effective(caller, tenant, Utc::now())?;
        let permissions = vec![Permission::ResourceRead, Permission::ResourceDraft];
        if !current.covers(&tenant_scope())
            || permissions
                .iter()
                .any(|permission| !current.permits(*permission))
        {
            return Err(ErrorCode::Forbidden);
        }
        Ok(Self {
            credential: current.credential.id.clone(),
            credential_revision: current.credential.revision.clone(),
            principal: current.credential.principal.clone(),
            binding: current.binding.id.clone(),
            credential_scope: current.credential.scope.clone(),
            binding_scope: current.binding.scope.clone(),
            permissions,
            credential_expires: current.credential.expires.wire.clone(),
            binding_expires: current.binding.expires.wire.clone(),
        })
    }

    pub(in crate::tenancy) fn principal(&self) -> &PrincipalId {
        &self.principal
    }

    pub(in crate::tenancy) fn caller(&self) -> Caller {
        Caller::Credential(self.credential.clone())
    }

    pub(in crate::tenancy) fn check(
        &self,
        state: &Snapshot,
        tenant: &TenantId,
        dependencies: &[ResourceRef],
    ) -> Result<(), ErrorCode> {
        let now = Utc::now();
        let current = state.effective(&self.caller(), tenant, now)?;
        if current.credential.revision != self.credential_revision
            || current.credential.principal != self.principal
            || current.binding.id != self.binding
            || !scope_subset(&tenant_scope(), &self.credential_scope, tenant)
            || !scope_subset(&tenant_scope(), &self.binding_scope, tenant)
            || !current.covers(&tenant_scope())
        {
            return Err(ErrorCode::Forbidden);
        }
        for expiry in [&self.credential_expires, &self.binding_expires] {
            let expiry = chrono::DateTime::parse_from_rfc3339(expiry.as_str())
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            if expiry <= now {
                return Err(ErrorCode::Forbidden);
            }
        }
        for permission in [Permission::ResourceRead, Permission::ResourceDraft] {
            if !self.permissions.contains(&permission) || !current.permits(permission) {
                return Err(ErrorCode::Forbidden);
            }
        }
        for reference in dependencies {
            let requested = GrantScope::Resources {
                resources: vec![reference.key()],
            };
            if !covers_key(&self.credential_scope, tenant, &reference.key())
                || !covers_key(&self.binding_scope, tenant, &reference.key())
                || !current.covers(&requested)
            {
                return Err(ErrorCode::Forbidden);
            }
        }
        Ok(())
    }
}

/// No native execution ownership or cleanup grant is carried by this read identity.
/// Construction stays inside H after loading a validated durable registration.
pub(crate) struct ReadIdentity {
    registration: Arc<Registration>,
    snapshot: Arc<RwLock<Arc<Snapshot>>>,
    live: Arc<AtomicBool>,
    proof: Arc<Proof>,
    deadline: Instant,
    database: sea_orm::DatabaseConnection,
    startup: Option<Arc<crate::tenancy::origin::StartupOwner>>,
    _stopped: Arc<crate::tenancy::hosts::StoppedRead>,
}

impl ReadIdentity {
    pub(in crate::tenancy) fn checked(
        registration: Arc<Registration>,
        worker: &crate::tenancy::actor::Worker,
        deadline: Instant,
        stopped: Arc<crate::tenancy::hosts::StoppedRead>,
    ) -> Result<Self, ErrorCode> {
        let value = Self {
            registration,
            snapshot: worker.snapshot.clone(),
            live: worker.live.0.clone(),
            proof: worker.proof.clone(),
            deadline,
            database: worker.db.clone(),
            startup: worker.startup.clone(),
            _stopped: stopped,
        };
        value.check(&[])?;
        Ok(value)
    }
    pub(crate) fn check_source(
        &self,
        loaded: &crate::managed_store::owned::Loaded,
    ) -> Result<(), ErrorCode> {
        use crate::tenancy::session::recovery::Phase;
        self.check(&[])?;
        if let Some(metadata) = &loaded.metadata {
            metadata
                .validate()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            if metadata.phase != Phase::Released || metadata.workspace_grant.is_some() {
                return Err(ErrorCode::CapabilityMismatch);
            }
        }
        match (&self.startup, &loaded.metadata) {
            (Some(startup), Some(metadata)) => {
                let current = startup
                    .stamp(&self.proof)
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                if current != metadata.origin {
                    startup
                        .excludes(&self.proof, &metadata.origin)
                        .map_err(|_| ErrorCode::CapabilityMismatch)?;
                }
            }
            // The old prepared-service fixtures have no claim. Production's
            // claimed Service requires the original native ownership stamp.
            (None, None) => {}
            _ => return Err(ErrorCode::CapabilityMismatch),
        }
        Ok(())
    }

    pub(crate) fn database(&self) -> &sea_orm::DatabaseConnection {
        &self.database
    }

    pub(crate) fn source_revision(&self) -> &RevisionId {
        &self.registration.input.expected_active_revision
    }

    pub(crate) fn storage_identity(&self) -> (&InstallationId, &TenantId, &PrincipalId) {
        (
            &self.registration.input.installation,
            &self.registration.input.tenant,
            self.registration.ceiling.principal(),
        )
    }

    pub(crate) fn check(&self, dependencies: &[ResourceRef]) -> Result<(), ErrorCode> {
        if Instant::now() >= self.deadline {
            return Err(ErrorCode::CapacityExceeded);
        }
        if !self.live.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let state = self
            .snapshot
            .read()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let input = &self.registration.input;
        if state.installation != input.installation
            || state.active_configurations.get(&input.tenant)
                != Some(&input.expected_active_revision)
        {
            return Err(ErrorCode::RevisionConflict);
        }
        self.registration
            .ceiling
            .check(&state, &input.tenant, dependencies)
    }

    pub(in crate::tenancy) fn origin(&self, proof: &Arc<Proof>) -> bool {
        Arc::ptr_eq(&self.proof, proof)
    }
}
