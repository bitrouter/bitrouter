//! Shared authority decoders for legacy snapshots and indexed directory reads.

use super::{Binding, Credential, Principal, Tenant, enum_from, json_from, string};
use crate::tenancy::model::{Deadline, ManagedBy};
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{authority::GrantScope, validation::Validate};
use sea_orm::QueryResult;

pub(super) fn tenant_from(row: &QueryResult) -> Result<Tenant> {
    let created_at = string(row, "created_at")?;
    chrono::DateTime::parse_from_rfc3339(&created_at)?;
    Ok(Tenant {
        id: string(row, "id")?.try_into()?,
        revision: string(row, "revision")?.try_into()?,
        state: enum_from(row, "state")?,
        created_at,
    })
}

pub(super) fn principal_from(row: &QueryResult) -> Result<Principal> {
    let issuer = string(row, "issuer")?;
    let subject = string(row, "subject")?;
    ensure!(
        !issuer.is_empty()
            && issuer.chars().count() <= 256
            && !subject.is_empty()
            && subject.chars().count() <= 256,
        "invalid principal identity"
    );
    let created_at = string(row, "created_at")?;
    chrono::DateTime::parse_from_rfc3339(&created_at)?;
    Ok(Principal {
        id: string(row, "id")?.try_into()?,
        revision: string(row, "revision")?.try_into()?,
        issuer,
        subject,
        created_at,
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
    let created_at = string(row, "created_at")?;
    chrono::DateTime::parse_from_rfc3339(&created_at)?;
    let key_hash = string(row, "key_hash")?;
    let _: bitrouter_ai::tenancy::identity::Digest = key_hash.clone().try_into()?;
    let permissions: Vec<_> = json_from(row, "permissions_json")?;
    ensure!(
        permissions.len() <= 10
            && permissions
                .iter()
                .enumerate()
                .all(|(index, permission)| !permissions[..index].contains(permission)),
        "invalid credential permissions"
    );
    let audit_reader: bool = row.try_get("", "audit_reader")?;
    let binding: Option<String> = row.try_get("", "binding_id")?;
    ensure!(
        if audit_reader {
            binding.is_none() && permissions.is_empty()
        } else {
            binding.is_some() && !permissions.is_empty()
        },
        "invalid credential grant"
    );
    Ok(Credential {
        id: string(row, "id")?.try_into()?,
        tenant: string(row, "tenant_id")?.try_into()?,
        principal: string(row, "principal_id")?.try_into()?,
        binding: binding.map(TryInto::try_into).transpose()?,
        revision: string(row, "revision")?.try_into()?,
        key_hash,
        state: enum_from(row, "state")?,
        audit_reader,
        permissions,
        scope,
        expires: Deadline::parse(string(row, "expires_at")?.try_into()?)?,
        created_at,
    })
}
