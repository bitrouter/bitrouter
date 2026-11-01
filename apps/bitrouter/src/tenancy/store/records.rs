//! Shared authority decoders for legacy snapshots and indexed directory reads.

use super::{Binding, Credential, Principal, Tenant, enum_from, json_from, string};
use crate::tenancy::model::{Deadline, ManagedBy};
use anyhow::Result;
use bitrouter_ai::tenancy::{authority::GrantScope, validation::Validate};
use sea_orm::QueryResult;

pub(super) fn tenant_from(row: &QueryResult) -> Result<Tenant> {
    Ok(Tenant {
        id: string(row, "id")?.try_into()?,
        revision: string(row, "revision")?.try_into()?,
        state: enum_from(row, "state")?,
        created_at: string(row, "created_at")?,
    })
}

pub(super) fn principal_from(row: &QueryResult) -> Result<Principal> {
    Ok(Principal {
        id: string(row, "id")?.try_into()?,
        revision: string(row, "revision")?.try_into()?,
        issuer: string(row, "issuer")?,
        subject: string(row, "subject")?,
        created_at: string(row, "created_at")?,
    })
}

pub(super) fn binding_from(row: &QueryResult) -> Result<Binding> {
    let owner: Option<String> = row.try_get("", "management_owner")?;
    let class: Option<String> = row.try_get("", "management_class")?;
    let managed_by = match (owner, class) {
        (Some(owner), Some(class)) => Some(ManagedBy {
            principal: owner.try_into()?,
            class: serde_json::from_value(serde_json::Value::String(class))?,
        }),
        (None, None) => None,
        _ => anyhow::bail!("inconsistent binding management authority"),
    };
    let scope: GrantScope = json_from(row, "scope_json")?;
    scope.validate()?;
    Ok(Binding {
        tenant: string(row, "tenant_id")?.try_into()?,
        id: string(row, "id")?.try_into()?,
        principal: string(row, "principal_id")?.try_into()?,
        revision: string(row, "revision")?.try_into()?,
        role: enum_from(row, "role")?,
        scope,
        expires: Deadline::parse(string(row, "expires_at")?.try_into()?)?,
        managed_by,
    })
}

pub(super) fn credential_from(row: &QueryResult) -> Result<Credential> {
    let scope: GrantScope = json_from(row, "scope_json")?;
    scope.validate()?;
    Ok(Credential {
        id: string(row, "id")?.try_into()?,
        tenant: string(row, "tenant_id")?.try_into()?,
        principal: string(row, "principal_id")?.try_into()?,
        binding: row
            .try_get::<Option<String>>("", "binding_id")?
            .map(TryInto::try_into)
            .transpose()?,
        revision: string(row, "revision")?.try_into()?,
        key_hash: string(row, "key_hash")?,
        state: enum_from(row, "state")?,
        audit_reader: row.try_get("", "audit_reader")?,
        permissions: json_from(row, "permissions_json")?,
        scope,
        expires: Deadline::parse(string(row, "expires_at")?.try_into()?)?,
        created_at: string(row, "created_at")?,
    })
}
