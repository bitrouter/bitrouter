//! Authority mutations run only inside the single owning command actor.

use base64::Engine;
use bitrouter_ai::tenancy::{
    authority::{CredentialIssued, CredentialState, GrantScope, Permission, TenantState},
    configuration::ConfigurationRevision,
    identity::{CredentialId, PrincipalId, ResourceId, RevisionId, TenantId},
    operation::ErrorCode,
};
use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;

use super::{
    authority::{has_pools, scope_subset},
    command::Command,
    configuration,
    configuration_store::{self, StoredConfiguration},
    model::{
        AuthorityClass, Binding, Caller, Credential, Deadline, ManagedBy, Principal, ReadContext,
        Snapshot, Tenant,
    },
    store::{self, Change},
};

type Result<T> = std::result::Result<T, ErrorCode>;

pub(super) struct Prepared {
    pub context: ReadContext,
    pub credential: Option<Credential>,
    pub configuration: Option<StoredConfiguration>,
}

fn operator(caller: &Caller) -> Result<()> {
    if matches!(caller, Caller::Installation(_)) {
        Ok(())
    } else {
        Err(ErrorCode::Forbidden)
    }
}

fn active(state: &Snapshot, tenant: &TenantId) -> Result<()> {
    match state.tenants.get(tenant) {
        None => Err(ErrorCode::NotFound),
        Some(tenant) if tenant.state != TenantState::Active => Err(ErrorCode::TenantInactive),
        Some(_) => Ok(()),
    }
}

pub(super) fn tenant_scope() -> GrantScope {
    GrantScope::Tenant {
        operator_pools: Vec::new(),
    }
}

fn scoped_context(
    caller: &Caller,
    tenant: &TenantId,
    permission: Permission,
    scope: &GrantScope,
) -> ReadContext {
    if matches!(caller, Caller::Installation(_)) {
        ReadContext::Installation
    } else {
        ReadContext::Tenant {
            tenant: tenant.clone(),
            permission,
            scopes: vec![scope.clone()],
        }
    }
}

/// Authorization precedes CAS details and capacity consumption. Replay uses its
/// saved read context instead; it cannot manufacture new authorization here.
pub(super) async fn prepare(
    db: &DatabaseConnection,
    state: &Snapshot,
    resources: &super::resources::Resources,
    caller: &Caller,
    command: &Command,
    now: DateTime<Utc>,
) -> Result<Prepared> {
    let mut credential = None;
    let mut retained = None;
    let context = match command {
        Command::Tenant(..)
        | Command::Principal(..)
        | Command::AuditReader(..)
        | Command::Detach(..) => {
            operator(caller)?;
            ReadContext::Installation
        }
        Command::Configuration(tenant, input) => {
            let value =
                configuration::prepare(db, resources, state, caller, tenant, &input.document, now)
                    .await?;
            let context = configuration::read_context(caller, tenant, &value.dependencies, false);
            retained = Some(value);
            context
        }
        Command::Activate(tenant, input) => {
            configuration::authorize(state, caller, tenant, Permission::PolicyActivate, &[], now)?;
            let value = configuration_store::configuration(db, tenant, &input.revision_id)
                .await
                .map_err(|_| ErrorCode::StorageUnavailable)?
                .ok_or(ErrorCode::NotFound)?;
            configuration::authorize(
                state,
                caller,
                tenant,
                Permission::ResourceRead,
                &value.dependencies,
                Utc::now(),
            )?;
            resources.validate_executable(&value)?;
            configuration::validate_profile(&value.revision.document)?;
            let context = configuration::read_context(caller, tenant, &value.dependencies, true);
            retained = Some(value);
            context
        }
        Command::Binding(tenant, id, input) => {
            state.authorize(caller, tenant, Permission::BindingManage, &input.scope, now)?;
            if let Some(old) = state.bindings.get(&(tenant.clone(), id.clone())) {
                state.authorize(caller, tenant, Permission::BindingManage, &old.scope, now)?;
            }
            if has_pools(&input.scope) {
                operator(caller)?;
            }
            if !matches!(caller, Caller::Installation(_)) {
                let effective = state.effective(caller, tenant, now)?;
                let expiry = Deadline::parse(input.expires_at.clone())
                    .map_err(|_| ErrorCode::InvalidRequest)?;
                if !input
                    .role
                    .permissions()
                    .iter()
                    .all(|permission| effective.permits(*permission))
                    || !effective.allows_expiry(expiry.instant)
                {
                    return Err(ErrorCode::Forbidden);
                }
            }
            let mut context =
                scoped_context(caller, tenant, Permission::BindingManage, &input.scope);
            if let ReadContext::Tenant { scopes, .. } = &mut context
                && let Some(old) = state.bindings.get(&(tenant.clone(), id.clone()))
                && old.scope != input.scope
            {
                scopes.push(old.scope.clone());
            }
            context
        }
        Command::Issue(tenant, input) => {
            state.authorize(
                caller,
                tenant,
                Permission::CredentialIssue,
                &input.scope,
                now,
            )?;
            if !matches!(caller, Caller::Installation(_)) {
                let effective = state.effective(caller, tenant, now)?;
                let expiry = Deadline::parse(input.expires_at.clone())
                    .map_err(|_| ErrorCode::InvalidRequest)?;
                if !input
                    .permissions
                    .iter()
                    .all(|permission| effective.permits(*permission))
                    || !effective.allows_expiry(expiry.instant)
                {
                    return Err(ErrorCode::Forbidden);
                }
            }
            let mut context =
                scoped_context(caller, tenant, Permission::CredentialManage, &input.scope);
            if let Some(id) = &input.rotate_credential_id {
                let old = state
                    .credentials
                    .get(id)
                    .filter(|credential| &credential.tenant == tenant)
                    .ok_or(ErrorCode::NotFound)?;
                state.authorize(
                    caller,
                    tenant,
                    Permission::CredentialManage,
                    &old.scope,
                    now,
                )?;
                if let ReadContext::Tenant { scopes, .. } = &mut context
                    && old.scope != input.scope
                {
                    scopes.push(old.scope.clone());
                }
            }
            context
        }
        Command::Revoke(tenant, id, _) => {
            credential = store::credential(db, id.as_str())
                .await
                .map_err(|_| ErrorCode::StorageUnavailable)?;
            let target = credential
                .as_ref()
                .filter(|credential| &credential.tenant == tenant)
                .ok_or(ErrorCode::NotFound)?;
            if target.audit_reader {
                operator(caller)?;
            }
            state.authorize(
                caller,
                tenant,
                Permission::CredentialManage,
                &target.scope,
                now,
            )?;
            scoped_context(caller, tenant, Permission::CredentialManage, &target.scope)
        }
    };
    Ok(Prepared {
        context,
        credential,
        configuration: retained,
    })
}

pub(super) struct Applied {
    pub changes: Vec<Change>,
    pub resource: ResourceId,
    pub revision: RevisionId,
    pub issued: Option<CredentialIssued>,
    pub configuration: Option<ConfigurationRevision>,
}

fn revision() -> Result<RevisionId> {
    uuid::Uuid::new_v4()
        .to_string()
        .try_into()
        .map_err(|_| ErrorCode::AuthorityUnavailable)
}

fn cas(current: Option<&RevisionId>, expected: Option<&RevisionId>) -> Result<()> {
    if current == expected {
        Ok(())
    } else {
        Err(ErrorCode::RevisionConflict)
    }
}

fn binding_key(
    tenant: &TenantId,
    id: &bitrouter_ai::tenancy::identity::BindingId,
) -> (TenantId, bitrouter_ai::tenancy::identity::BindingId) {
    (tenant.clone(), id.clone())
}

fn apply_credential(state: &mut Snapshot, credential: &Credential, now: DateTime<Utc>) {
    if let Some(old) = state.credentials.remove(&credential.id) {
        state.credential_hashes.remove(&old.key_hash);
    }
    if credential.state == CredentialState::Active && credential.expires.instant > now {
        state
            .credential_hashes
            .insert(credential.key_hash.clone(), credential.id.clone());
        state
            .credentials
            .insert(credential.id.clone(), credential.clone());
    }
}

fn credential_capacity(
    state: &Snapshot,
    tenant: &TenantId,
    principal: &PrincipalId,
    rotating: bool,
    now: DateTime<Utc>,
) -> Result<()> {
    let live = state
        .credentials
        .values()
        .filter(|credential| credential.expires.instant > now)
        .collect::<Vec<_>>();
    let replacing = usize::from(rotating);
    if live.len().saturating_sub(replacing) >= 256
        || live
            .iter()
            .filter(|credential| &credential.tenant == tenant && &credential.principal == principal)
            .count()
            .saturating_sub(replacing)
            >= 2
    {
        return Err(ErrorCode::CapacityExceeded);
    }
    let tenant_principals = live
        .iter()
        .filter(|credential| &credential.tenant == tenant)
        .map(|credential| &credential.principal)
        .chain(
            state
                .bindings
                .values()
                .filter(|binding| &binding.tenant == tenant)
                .map(|binding| &binding.principal),
        )
        .collect::<std::collections::BTreeSet<_>>();
    if !tenant_principals.contains(principal) && tenant_principals.len() >= 4 {
        return Err(ErrorCode::CapacityExceeded);
    }
    Ok(())
}

/// Called on a private candidate snapshot; any error discards all candidate edits.
pub(super) fn apply(
    state: &mut Snapshot,
    caller: &Caller,
    principal: &PrincipalId,
    command: &Command,
    prepared: &Prepared,
    now: DateTime<Utc>,
) -> Result<Applied> {
    if matches!(caller, Caller::Credential(_)) {
        let credential = state.credential(caller, now)?;
        state.effective(caller, &credential.tenant, now)?;
    }
    let next_revision = revision()?;
    let mut issued = None;
    let (resource, changes) = match command {
        Command::Configuration(..) | Command::Activate(..) => {
            return configuration::apply(state, caller, command, prepared, now);
        }
        Command::Tenant(id, input) => {
            let old = state.tenants.get(id);
            cas(
                old.map(|tenant| &tenant.revision),
                input.expected_revision.as_ref(),
            )?;
            if old.is_none() && state.tenants.len() >= 32 {
                return Err(ErrorCode::CapacityExceeded);
            }
            let tenant = Tenant {
                id: id.clone(),
                revision: next_revision.clone(),
                state: input.state,
                created_at: old
                    .map_or_else(|| now.to_rfc3339(), |tenant| tenant.created_at.clone()),
            };
            state.tenants.insert(id.clone(), tenant.clone());
            (id.as_str(), vec![Change::Tenant(tenant)])
        }
        Command::Principal(id, input) => {
            if id.as_str().starts_with("operator.") {
                return Err(ErrorCode::InvalidRequest);
            }
            let old = state.principals.get(id);
            cas(
                old.map(|principal| &principal.revision),
                input.expected_revision.as_ref(),
            )?;
            if old.is_some_and(|old| old.issuer != input.issuer || old.subject != input.subject)
                || state.principals.values().any(|principal| {
                    principal.id != *id
                        && principal.issuer == input.issuer
                        && principal.subject == input.subject
                })
            {
                return Err(ErrorCode::RevisionConflict);
            }
            if old.is_none() && state.principals.len() >= 128 {
                return Err(ErrorCode::CapacityExceeded);
            }
            let value = Principal {
                id: id.clone(),
                revision: next_revision.clone(),
                issuer: input.issuer.clone(),
                subject: input.subject.clone(),
                created_at: old.map_or_else(|| now.to_rfc3339(), |old| old.created_at.clone()),
            };
            state.principals.insert(id.clone(), value.clone());
            (id.as_str(), vec![Change::Principal(value)])
        }
        Command::Binding(tenant, id, input) => {
            active(state, tenant)?;
            if !state.principals.contains_key(&input.principal_id) {
                return Err(ErrorCode::NotFound);
            }
            let expires =
                Deadline::parse(input.expires_at.clone()).map_err(|_| ErrorCode::InvalidRequest)?;
            if expires.instant <= now {
                return Err(ErrorCode::InvalidRequest);
            }
            // Even installation operators must use an explicit pool namespace;
            // a tenant binding can never contain another tenant's resources.
            if let GrantScope::Resources { resources } = &input.scope
                && resources.iter().any(|key| {
                    key.owner_kind == bitrouter_ai::tenancy::authority::OwnerKind::Tenant
                        && key.owner_id.as_str() != tenant.as_str()
                })
            {
                return Err(ErrorCode::Forbidden);
            }
            let old = state.bindings.get(&binding_key(tenant, id));
            cas(
                old.map(|binding| &binding.revision),
                input.expected_revision.as_ref(),
            )?;
            let class = if matches!(caller, Caller::Installation(_)) {
                AuthorityClass::Installation
            } else {
                AuthorityClass::Tenant
            };
            let managed_by = if let Some(old) = old {
                if old.managed_by.as_ref().map(|owner| &owner.principal)
                    != input.expected_management_owner.as_ref()
                {
                    return Err(ErrorCode::RevisionConflict);
                }
                match &old.managed_by {
                    Some(owner) if owner.principal == *principal && owner.class == class => {
                        Some(owner.clone())
                    }
                    None if class == AuthorityClass::Installation => None,
                    _ => return Err(ErrorCode::Forbidden),
                }
            } else {
                if state.bindings.len() >= 256 {
                    return Err(ErrorCode::CapacityExceeded);
                }
                Some(ManagedBy {
                    principal: principal.clone(),
                    class,
                })
            };
            let members = state
                .bindings
                .values()
                .filter(|binding| &binding.tenant == tenant && binding.id != *id)
                .map(|binding| &binding.principal)
                .chain(
                    state
                        .credentials
                        .values()
                        .filter(|credential| {
                            &credential.tenant == tenant && credential.expires.instant > now
                        })
                        .map(|credential| &credential.principal),
                )
                .collect::<std::collections::BTreeSet<_>>();
            if !members.contains(&input.principal_id) && members.len() >= 4 {
                return Err(ErrorCode::CapacityExceeded);
            }
            let binding = Binding {
                tenant: tenant.clone(),
                id: id.clone(),
                principal: input.principal_id.clone(),
                revision: next_revision.clone(),
                role: input.role,
                scope: input.scope.clone(),
                expires,
                managed_by,
            };
            state
                .bindings
                .insert(binding_key(tenant, id), binding.clone());
            (id.as_str(), vec![Change::Binding(binding)])
        }
        Command::Detach(tenant, id, input) => {
            let binding = state
                .bindings
                .get_mut(&binding_key(tenant, id))
                .ok_or(ErrorCode::NotFound)?;
            cas(Some(&binding.revision), Some(&input.expected_revision))?;
            binding.managed_by = None;
            binding.revision = next_revision.clone();
            (id.as_str(), vec![Change::Binding(binding.clone())])
        }
        Command::Revoke(_, id, input) => {
            let mut credential = prepared.credential.clone().ok_or(ErrorCode::NotFound)?;
            cas(Some(&credential.revision), Some(&input.expected_revision))?;
            if credential.state == CredentialState::Revoked {
                return Ok(Applied {
                    changes: vec![],
                    resource: id
                        .as_str()
                        .try_into()
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?,
                    revision: credential.revision,
                    issued: None,
                    configuration: None,
                });
            }
            credential.state = CredentialState::Revoked;
            credential.revision = next_revision.clone();
            apply_credential(state, &credential, now);
            (id.as_str(), vec![Change::Credential(credential)])
        }
        Command::Issue(tenant, input) => {
            active(state, tenant)?;
            if input.expected_authority_revision != state.revision {
                return Err(ErrorCode::RevisionConflict);
            }
            let binding = state
                .bindings
                .get(&binding_key(tenant, &input.binding_id))
                .ok_or(ErrorCode::NotFound)?;
            let expires =
                Deadline::parse(input.expires_at.clone()).map_err(|_| ErrorCode::InvalidRequest)?;
            if expires.instant <= now {
                return Err(ErrorCode::InvalidRequest);
            }
            if binding.principal != input.principal_id
                || expires.instant > binding.expires.instant
                || !input
                    .permissions
                    .iter()
                    .all(|permission| binding.role.permissions().contains(permission))
                || !scope_subset(&input.scope, &binding.scope, tenant)
                || (has_pools(&input.scope)
                    && !binding
                        .managed_by
                        .as_ref()
                        .is_some_and(|owner| owner.class == AuthorityClass::Installation))
            {
                return Err(ErrorCode::Forbidden);
            }
            let old = if let Some(id) = &input.rotate_credential_id {
                let predecessor = state
                    .credentials
                    .get(id)
                    .filter(|credential| credential.expires.instant > now)
                    .ok_or(ErrorCode::NotFound)?;
                if predecessor.tenant != *tenant
                    || predecessor.principal != input.principal_id
                    || predecessor.binding.as_ref() != Some(&input.binding_id)
                    || predecessor.audit_reader
                    || !scope_subset(&input.scope, &predecessor.scope, tenant)
                    || !input
                        .permissions
                        .iter()
                        .all(|permission| predecessor.permissions.contains(permission))
                    || expires.instant > predecessor.expires.instant
                {
                    return Err(ErrorCode::Forbidden);
                }
                Some(predecessor.clone())
            } else {
                None
            };
            credential_capacity(state, tenant, &input.principal_id, old.is_some(), now)?;
            let (credential, response) = issue(
                state,
                command,
                &next_revision,
                NewCredential {
                    tenant: tenant.clone(),
                    principal: input.principal_id.clone(),
                    binding: Some(input.binding_id.clone()),
                    audit_reader: false,
                    permissions: input.permissions.clone(),
                    scope: input.scope.clone(),
                    expires,
                },
                now,
            )?;
            let id = credential
                .id
                .as_str()
                .try_into()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            let mut changes = Vec::new();
            if let Some(mut old) = old {
                old.state = CredentialState::Revoked;
                old.revision = revision()?;
                apply_credential(state, &old, now);
                changes.push(Change::Credential(old));
            }
            apply_credential(state, &credential, now);
            changes.push(Change::Credential(credential));
            return Ok(Applied {
                changes,
                resource: id,
                revision: next_revision,
                issued: Some(response),
                configuration: None,
            });
        }
        Command::AuditReader(tenant, input) => {
            if input.expected_authority_revision != state.revision {
                return Err(ErrorCode::RevisionConflict);
            }
            if !state.tenants.contains_key(tenant)
                || !state.principals.contains_key(&input.principal_id)
            {
                return Err(ErrorCode::NotFound);
            }
            let expires =
                Deadline::parse(input.expires_at.clone()).map_err(|_| ErrorCode::InvalidRequest)?;
            if expires.instant <= now {
                return Err(ErrorCode::InvalidRequest);
            }
            credential_capacity(state, tenant, &input.principal_id, false, now)?;
            let (credential, response) = issue(
                state,
                command,
                &next_revision,
                NewCredential {
                    tenant: tenant.clone(),
                    principal: input.principal_id.clone(),
                    binding: None,
                    audit_reader: true,
                    permissions: Vec::new(),
                    scope: tenant_scope(),
                    expires,
                },
                now,
            )?;
            let id = credential
                .id
                .as_str()
                .try_into()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            apply_credential(state, &credential, now);
            issued = Some(response);
            return Ok(Applied {
                changes: vec![Change::Credential(credential)],
                resource: id,
                revision: next_revision,
                issued,
                configuration: None,
            });
        }
    };
    Ok(Applied {
        changes,
        resource: resource
            .try_into()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?,
        revision: next_revision,
        issued,
        configuration: None,
    })
}

struct NewCredential {
    tenant: TenantId,
    principal: PrincipalId,
    binding: Option<bitrouter_ai::tenancy::identity::BindingId>,
    audit_reader: bool,
    permissions: Vec<Permission>,
    scope: GrantScope,
    expires: Deadline,
}

fn issue(
    state: &Snapshot,
    command: &Command,
    revision: &RevisionId,
    input: NewCredential,
    now: DateTime<Utc>,
) -> Result<(Credential, CredentialIssued)> {
    let id: CredentialId = uuid::Uuid::new_v4()
        .to_string()
        .try_into()
        .map_err(|_| ErrorCode::AuthorityUnavailable)?;
    let random: [u8; 32] = rand::random();
    let secret = format!(
        "brtk_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
    );
    let credential = Credential {
        id: id.clone(),
        tenant: input.tenant.clone(),
        principal: input.principal.clone(),
        binding: input.binding,
        revision: revision.clone(),
        key_hash: crate::auth::keys::hash_key(&secret),
        state: CredentialState::Active,
        audit_reader: input.audit_reader,
        permissions: input.permissions,
        scope: input.scope,
        expires: input.expires.clone(),
        created_at: now.to_rfc3339(),
    };
    let response = CredentialIssued {
        operation_id: command.operation_id().clone(),
        credential_id: id,
        principal_id: input.principal,
        tenant_id: input.tenant,
        authority_revision: state.revision,
        secret: Some(secret),
        expires_at: input.expires.wire,
        credential_revision: revision.clone(),
    };
    Ok((credential, response))
}
