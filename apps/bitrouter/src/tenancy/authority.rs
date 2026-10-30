//! Current-grant intersection. No credential carries a cached entitlement.

use bitrouter_ai::tenancy::{
    authority::{CredentialState, GrantScope, OwnerKind, Permission, ResourceKey, TenantState},
    identity::{PrincipalId, TenantId},
    operation::ErrorCode,
};
use chrono::{DateTime, Utc};

use super::model::{AuthorityClass, Binding, Caller, Credential, ReadContext, Snapshot};

pub(super) struct Effective<'a> {
    pub credential: &'a Credential,
    pub binding: &'a Binding,
}

impl Effective<'_> {
    pub fn permits(&self, permission: Permission) -> bool {
        self.credential.permissions.contains(&permission)
            && self.binding.role.permissions().contains(&permission)
    }

    pub fn covers(&self, scope: &GrantScope) -> bool {
        let tenant = &self.credential.tenant;
        scope_subset(scope, &self.credential.scope, tenant)
            && scope_subset(scope, &self.binding.scope, tenant)
            && (!has_pools(scope)
                || self
                    .binding
                    .managed_by
                    .as_ref()
                    .is_some_and(|owner| owner.class == AuthorityClass::Installation))
    }

    pub fn allows_expiry(&self, expiry: DateTime<Utc>) -> bool {
        expiry <= self.credential.expires.instant && expiry <= self.binding.expires.instant
    }
}

impl Snapshot {
    pub fn credential(
        &self,
        caller: &Caller,
        now: DateTime<Utc>,
    ) -> Result<&Credential, ErrorCode> {
        let Caller::Credential(id) = caller else {
            return Err(ErrorCode::Forbidden);
        };
        self.credentials
            .get(id)
            .filter(|credential| {
                credential.state == CredentialState::Active && credential.expires.instant > now
            })
            .ok_or(ErrorCode::Unauthorized)
    }

    pub fn principal<'a>(
        &'a self,
        caller: &'a Caller,
        now: DateTime<Utc>,
    ) -> Result<&'a PrincipalId, ErrorCode> {
        match caller {
            Caller::Installation(principal) => Ok(principal),
            Caller::Credential(_) => Ok(&self.credential(caller, now)?.principal),
        }
    }

    pub fn effective(
        &self,
        caller: &Caller,
        tenant: &TenantId,
        now: DateTime<Utc>,
    ) -> Result<Effective<'_>, ErrorCode> {
        let credential = self.credential(caller, now)?;
        if &credential.tenant != tenant || credential.audit_reader {
            return Err(ErrorCode::Forbidden);
        }
        if self.tenants.get(tenant).map(|tenant| tenant.state) != Some(TenantState::Active) {
            return Err(ErrorCode::TenantInactive);
        }
        let binding = credential
            .binding
            .as_ref()
            .and_then(|id| self.bindings.get(&(tenant.clone(), id.clone())))
            .filter(|binding| {
                binding.principal == credential.principal && binding.expires.instant > now
            })
            .ok_or(ErrorCode::Forbidden)?;
        Ok(Effective {
            credential,
            binding,
        })
    }

    pub fn authorize(
        &self,
        caller: &Caller,
        tenant: &TenantId,
        permission: Permission,
        scope: &GrantScope,
        now: DateTime<Utc>,
    ) -> Result<(), ErrorCode> {
        if matches!(caller, Caller::Installation(_)) {
            return Ok(());
        }
        let effective = self.effective(caller, tenant, now)?;
        if effective.permits(permission) && effective.covers(scope) {
            Ok(())
        } else {
            Err(ErrorCode::Forbidden)
        }
    }

    pub fn authorize_read(
        &self,
        caller: &Caller,
        context: &ReadContext,
        now: DateTime<Utc>,
    ) -> Result<(), ErrorCode> {
        match context {
            ReadContext::Installation if matches!(caller, Caller::Installation(_)) => Ok(()),
            ReadContext::Installation => Err(ErrorCode::Forbidden),
            ReadContext::Tenant {
                tenant,
                permission,
                scopes,
            } => {
                for scope in scopes {
                    self.authorize(caller, tenant, *permission, scope, now)?;
                }
                if scopes.is_empty() {
                    return Err(ErrorCode::Forbidden);
                }
                Ok(())
            }
        }
    }
}

pub(super) fn has_pools(scope: &GrantScope) -> bool {
    match scope {
        GrantScope::Tenant { operator_pools } => !operator_pools.is_empty(),
        GrantScope::Resources { resources } => resources
            .iter()
            .any(|key| key.owner_kind == OwnerKind::OperatorPool),
    }
}

pub(super) fn covers_key(scope: &GrantScope, tenant: &TenantId, key: &ResourceKey) -> bool {
    if key.owner_kind == OwnerKind::Tenant && key.owner_id.as_str() != tenant.as_str() {
        return false;
    }
    match scope {
        GrantScope::Tenant { operator_pools } => {
            key.owner_kind == OwnerKind::Tenant || operator_pools.contains(key)
        }
        GrantScope::Resources { resources } => resources.contains(key),
    }
}

pub(super) fn scope_subset(narrow: &GrantScope, broad: &GrantScope, tenant: &TenantId) -> bool {
    match narrow {
        GrantScope::Tenant { operator_pools } => {
            matches!(broad, GrantScope::Tenant { .. })
                && operator_pools.iter().all(|key| {
                    key.owner_kind == OwnerKind::OperatorPool && covers_key(broad, tenant, key)
                })
        }
        GrantScope::Resources { resources } => {
            !resources.is_empty() && resources.iter().all(|key| covers_key(broad, tenant, key))
        }
    }
}
