//! Private authority records. Wire values alone never establish a caller.

use std::collections::BTreeMap;

use bitrouter_ai::tenancy::{
    authority::{CredentialState, GrantScope, Permission, TenantRole, TenantState},
    identity::{
        AuthorityRevision, BindingId, CredentialId, InstallationId, PrincipalId, RevisionId,
        TenantId, Timestamp,
    },
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AuthorityClass {
    Installation,
    Tenant,
}

#[derive(Clone)]
pub(super) struct Tenant {
    pub id: TenantId,
    pub revision: RevisionId,
    pub state: TenantState,
    pub created_at: String,
}

#[derive(Clone)]
pub(super) struct Principal {
    pub id: PrincipalId,
    pub revision: RevisionId,
    pub issuer: String,
    pub subject: String,
    pub created_at: String,
}

#[derive(Clone)]
pub(super) struct Deadline {
    pub wire: Timestamp,
    pub instant: DateTime<Utc>,
}

impl Deadline {
    pub fn parse(wire: Timestamp) -> anyhow::Result<Self> {
        let instant = DateTime::parse_from_rfc3339(wire.as_str())?.with_timezone(&Utc);
        Ok(Self { wire, instant })
    }
}

#[derive(Clone)]
pub(super) struct ManagedBy {
    pub principal: PrincipalId,
    pub class: AuthorityClass,
}

#[derive(Clone)]
pub(super) struct Binding {
    pub tenant: TenantId,
    pub id: BindingId,
    pub principal: PrincipalId,
    pub revision: RevisionId,
    pub role: TenantRole,
    pub scope: GrantScope,
    pub expires: Deadline,
    pub managed_by: Option<ManagedBy>,
}

#[derive(Clone)]
pub(super) struct Credential {
    pub id: CredentialId,
    pub tenant: TenantId,
    pub principal: PrincipalId,
    pub binding: Option<BindingId>,
    pub revision: RevisionId,
    pub key_hash: String,
    pub state: CredentialState,
    pub audit_reader: bool,
    pub permissions: Vec<Permission>,
    pub scope: GrantScope,
    pub expires: Deadline,
    pub created_at: String,
}

#[derive(Clone)]
pub(super) struct Snapshot {
    pub installation: InstallationId,
    pub revision: AuthorityRevision,
    pub operation_ids: u32,
    pub safety_operation_ids: u32,
    pub saturated: bool,
    pub created_at: String,
    pub tenants: BTreeMap<TenantId, Tenant>,
    pub principals: BTreeMap<PrincipalId, Principal>,
    pub bindings: BTreeMap<(TenantId, BindingId), Binding>,
    /// Only active, unexpired credentials enter the authentication snapshot.
    /// Revoked history remains queryable through the protected store path.
    pub credentials: BTreeMap<CredentialId, Credential>,
    pub credential_hashes: BTreeMap<String, CredentialId>,
    /// Only the bounded activation pointers are cached, not every retained draft.
    pub active_configurations: BTreeMap<TenantId, RevisionId>,
    /// Managed heads and exact revocations; checked even for cached executions.
    pub resource_lifecycle: super::resources::lifecycle::State,
}

/// Authentication produces this internal identity; requests cannot deserialize it.
#[derive(Clone)]
pub(super) enum Caller {
    Installation(PrincipalId),
    Credential(CredentialId),
}

/// Persisted command-read authority; never accepted from an HTTP request.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum ReadContext {
    Installation,
    Tenant {
        tenant: TenantId,
        permission: Permission,
        scopes: Vec<GrantScope>,
    },
}
