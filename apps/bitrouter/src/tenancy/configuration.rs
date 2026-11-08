//! Tenant configuration authorization, separate from host configuration reload.

use bitrouter_ai::tenancy::{
    authority::{GrantScope, OwnerKind, Permission, TenantState},
    configuration::{ActiveRevision, Configuration, ConfigurationRevision, ResourceRef},
    identity::{RevisionId, TenantId},
    operation::ErrorCode,
};
use chrono::{DateTime, Utc};

use super::{
    Service,
    command::Command,
    configuration_store::{self, StoredConfiguration},
    error::Failure,
    model::{Caller, ReadContext, Snapshot},
    mutation::{Applied, Prepared},
    store::Change,
};

pub(super) async fn prepare(
    db: &impl sea_orm::ConnectionTrait,
    resources: &super::resources::Resources,
    state: &Snapshot,
    caller: &Caller,
    tenant: &TenantId,
    document: &Configuration,
    now: DateTime<Utc>,
) -> Result<StoredConfiguration, ErrorCode> {
    use sha2::{Digest, Sha256};
    authorize(
        state,
        caller,
        tenant,
        Permission::ResourceDraft,
        &references(document),
        now,
    )?;
    validate_profile(document)?;
    state.resource_admission.configuration(&document.routes)?;
    let revision_id: RevisionId = uuid::Uuid::new_v4()
        .to_string()
        .try_into()
        .map_err(|_| ErrorCode::AuthorityUnavailable)?;
    let compiled = resources
        .compile(db, tenant, &revision_id, document)
        .await?;
    for reference in &compiled.dependencies {
        super::resources::lifecycle::check_in(db, &state.installation, state.revision, reference)
            .await?;
    }
    authorize(
        state,
        caller,
        tenant,
        Permission::ResourceDraft,
        &compiled.dependencies,
        Utc::now(),
    )?;
    let mut value = serde_json::to_value(document).map_err(|_| ErrorCode::InvalidRequest)?;
    super::command::sort(&mut value);
    let bytes = serde_json::to_vec(&value).map_err(|_| ErrorCode::InvalidRequest)?;
    let content_digest = hex::encode(Sha256::digest(bytes))
        .try_into()
        .map_err(|_| ErrorCode::AuthorityUnavailable)?;
    Ok(StoredConfiguration {
        revision: ConfigurationRevision {
            tenant_id: tenant.clone(),
            revision_id,
            schema_version: 1,
            compiler_version: compiled.compiler_version,
            content_digest,
            document: document.clone(),
        },
        compiled: compiled.payload,
        dependencies: compiled.dependencies,
        creator: state.principal(caller, Utc::now())?.clone(),
        authority_revision: next_revision(state)?,
        created_at: now
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .try_into()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?,
    })
}

/// A retained document cannot enable facilities omitted from this binary.
pub(super) fn validate_profile(document: &Configuration) -> Result<(), ErrorCode> {
    if !cfg!(feature = "core")
        && (document.learning.mode != bitrouter_ai::tenancy::configuration::LearningMode::Off
            || document.learning.max_evaluations != 0
            || document.limits.learning_jobs != 0)
    {
        return Err(ErrorCode::InvalidRequest);
    }
    Ok(())
}

fn next_revision(
    state: &Snapshot,
) -> Result<bitrouter_ai::tenancy::identity::AuthorityRevision, ErrorCode> {
    state
        .revision
        .get()
        .checked_add(1)
        .and_then(|revision| revision.try_into().ok())
        .ok_or(ErrorCode::AuthorityUnavailable)
}

pub(super) fn apply(
    state: &mut Snapshot,
    caller: &Caller,
    command: &Command,
    prepared: &Prepared,
    now: DateTime<Utc>,
) -> Result<Applied, ErrorCode> {
    let retained = prepared
        .configuration
        .as_ref()
        .ok_or(ErrorCode::AuthorityUnavailable)?;
    let (tenant, expected, permission) = match command {
        Command::Configuration(tenant, input) => (
            tenant,
            input.expected_authority_revision,
            Permission::ResourceDraft,
        ),
        Command::Activate(tenant, input) => (
            tenant,
            input.expected_authority_revision,
            Permission::PolicyActivate,
        ),
        _ => return Err(ErrorCode::InvalidRequest),
    };
    authorize(
        state,
        caller,
        tenant,
        permission,
        if matches!(command, Command::Activate(..)) {
            &[]
        } else {
            &retained.dependencies
        },
        now,
    )?;
    if state.revision != expected {
        return Err(ErrorCode::RevisionConflict);
    }
    let revision = retained.revision.revision_id.clone();
    state
        .resource_admission
        .configuration(&retained.revision.document.routes)?;
    let mut configuration = None;
    let changes = match command {
        Command::Configuration(..) => {
            if retained.authority_revision != next_revision(state)? {
                return Err(ErrorCode::RevisionConflict);
            }
            configuration = Some(retained.revision.clone());
            vec![Change::Configuration(retained.clone())]
        }
        Command::Activate(_, input) => {
            if retained
                .dependencies
                .iter()
                .any(|reference| state.resource_lifecycle.revoked(reference))
            {
                return Err(ErrorCode::Forbidden);
            }
            authorize(
                state,
                caller,
                tenant,
                Permission::ResourceRead,
                &retained.dependencies,
                now,
            )?;
            if state.active_configurations.get(tenant) != input.expected_active_revision.as_ref() {
                return Err(ErrorCode::RevisionConflict);
            }
            let authority_revision = next_revision(state)?;
            state
                .active_configurations
                .insert(tenant.clone(), revision.clone());
            vec![Change::Activation {
                tenant: tenant.clone(),
                expected: input.expected_active_revision.clone(),
                revision: revision.clone(),
                authority_revision,
            }]
        }
        _ => return Err(ErrorCode::InvalidRequest),
    };
    Ok(Applied {
        resource: revision
            .as_str()
            .try_into()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?,
        revision,
        changes,
        issued: None,
        configuration,
    })
}

pub(super) fn references(document: &Configuration) -> Vec<ResourceRef> {
    document
        .routes
        .iter()
        .chain(document.policy.iter())
        .cloned()
        .collect()
}

/// A scoped configuration never turns an empty reference list into a wildcard.
/// Empty configurations require an explicit whole-tenant grant.
fn scopes(references: &[ResourceRef]) -> Vec<GrantScope> {
    if references.is_empty() {
        vec![super::mutation::tenant_scope()]
    } else {
        references
            .iter()
            .map(|reference| GrantScope::Resources {
                resources: vec![reference.key()],
            })
            .collect()
    }
}

pub(super) fn authorize(
    state: &Snapshot,
    caller: &Caller,
    tenant: &TenantId,
    permission: Permission,
    references: &[ResourceRef],
    now: DateTime<Utc>,
) -> Result<(), ErrorCode> {
    if matches!(caller, Caller::Credential(_)) {
        state.effective(caller, tenant, now)?;
    }
    match state.tenants.get(tenant) {
        None => return Err(ErrorCode::NotFound),
        Some(tenant)
            if tenant.state != TenantState::Active
                && !(matches!(caller, Caller::Installation(_))
                    && permission == Permission::ResourceRead) =>
        {
            return Err(ErrorCode::TenantInactive);
        }
        Some(_) => {}
    }
    // Even an installation operator cannot compile another tenant's private
    // resource into this tenant. Sharing requires an explicit operator pool.
    if references.iter().any(|reference| {
        reference.owner_kind == OwnerKind::Tenant && reference.owner_id.as_str() != tenant.as_str()
    }) {
        return Err(ErrorCode::Forbidden);
    }
    for scope in scopes(references) {
        state.authorize(caller, tenant, permission, &scope, now)?;
    }
    Ok(())
}

impl Service {
    pub(super) async fn active_revision(
        &self,
        caller: &Caller,
        tenant: &TenantId,
    ) -> Result<ActiveRevision, Failure> {
        let identity = self
            .authority_read(super::actor::AuthoritySubject::Tenant {
                caller: caller.clone(),
                tenant: tenant.clone(),
                active: true,
                resource: None,
            })
            .await?;
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| {
                let state = view.facts();
                authorize(
                    state,
                    caller,
                    tenant,
                    Permission::ResourceRead,
                    &[],
                    Utc::now(),
                )?;
                Ok(ActiveRevision {
                    tenant_id: tenant.clone(),
                    revision_id: state.active_configurations.get(tenant).cloned(),
                    authority_revision: state.revision,
                })
            })
            .map_err(Failure::from)
    }

    pub(super) async fn configuration(
        &self,
        caller: &Caller,
        tenant: &TenantId,
        revision: &RevisionId,
    ) -> Result<ConfigurationRevision, Failure> {
        let identity = self
            .authority_read(super::actor::AuthoritySubject::Tenant {
                caller: caller.clone(),
                tenant: tenant.clone(),
                active: false,
                resource: None,
            })
            .await?;
        // Reject foreign and inactive callers before consulting retained content.
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| {
                if matches!(caller, Caller::Credential(_))
                    && !view
                        .facts()
                        .effective(caller, tenant, Utc::now())?
                        .permits(Permission::ResourceRead)
                {
                    return Err(ErrorCode::Forbidden);
                }
                Ok(())
            })
            .map_err(Failure::from)?;
        let _read = self.read_permit()?;
        let retained = tokio::time::timeout(
            super::MANAGEMENT_TIMEOUT,
            configuration_store::configuration(&self.db, tenant, revision),
        )
        .await
        .map_err(|_| self.unavailable())?
        .map_err(|_| self.unavailable())?
        .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
        self.resources
            .validate_compiled(&retained)
            .map_err(|_| self.unavailable())?;
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| {
                let state = view.facts();
                authorize(
                    state,
                    caller,
                    tenant,
                    Permission::ResourceRead,
                    &retained.dependencies,
                    Utc::now(),
                )?;
                if retained.authority_revision > state.revision {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                Ok(())
            })
            .map_err(Failure::from)?;
        Ok(retained.revision)
    }
}

pub(super) fn read_context(
    caller: &Caller,
    tenant: &TenantId,
    references: &[ResourceRef],
    whole_tenant: bool,
) -> ReadContext {
    if matches!(caller, Caller::Installation(_)) {
        ReadContext::Installation
    } else {
        let mut required = scopes(references);
        if whole_tenant && !references.is_empty() {
            required.push(super::mutation::tenant_scope());
        }
        ReadContext::Tenant {
            tenant: tenant.clone(),
            permission: Permission::ResourceRead,
            scopes: required,
        }
    }
}
