//! Policy mutations inside a caller-owned authority/receipt transaction.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::operator::{ResourcePolicy, ResourcePut, TenantPolicy, TenantPut};
use sea_orm::DatabaseTransaction;

use super::{Rejection, Resource, Tenant, TenantLimits, validate_id};
use crate::gateway::shared::{
    authority::Authority,
    store::{Kind, NewRecord, Store, Stored},
};

pub(in crate::gateway::shared) fn validate_tenant(input: &TenantPut) -> Result<()> {
    validate_id(&input.tenant_id)?;
    limits(input).validate()
}

fn limits(input: &TenantPut) -> TenantLimits {
    TenantLimits {
        concurrent_executions: input.limits.concurrent_executions,
        requests_per_minute: input.limits.requests_per_minute,
        resource_pools: input.limits.resource_pools.clone(),
    }
}

pub(in crate::gateway::shared) fn validate_resource(input: &ResourcePut) -> Result<()> {
    validate_id(&input.resource_pool_id)?;
    ensure!(
        input.concurrent_attempts <= 1_000_000,
        "shared resource limit exceeds supported bound"
    );
    Ok(())
}

/// Caller holds the head fence. Occupancy and rate counters are never reset.
pub(in crate::gateway::shared) async fn put_tenant(
    authority: &Authority,
    tx: &DatabaseTransaction,
    input: &TenantPut,
) -> Result<TenantPolicy> {
    validate_tenant(input)?;
    let store = authority.store();
    let now = Store::now(tx).await?;
    let revision = match input.expected_revision {
        None => {
            ensure!(
                store
                    .create(
                        tx,
                        NewRecord {
                            kind: Kind::Tenant,
                            key: &input.tenant_id,
                            status: "active",
                            due_at_ms: None,
                            now_ms: now,
                            value: &Tenant {
                                limits: limits(input),
                                policy_revision: 1,
                                active: 0,
                                minute: now.div_euclid(60_000),
                                accepted: 0,
                            },
                        }
                    )
                    .await?,
                Rejection::AuthorityChanged
            );
            1
        }
        Some(expected) => {
            let mut tenant: Stored<Tenant> = store
                .lock_optional(tx, Kind::Tenant, &input.tenant_id)
                .await?
                .ok_or(Rejection::NotFound)?;
            ensure!(
                tenant.value.policy_revision == expected,
                Rejection::AuthorityChanged
            );
            tenant.value.policy_revision = expected
                .checked_add(1)
                .context("tenant policy revision exhausted")?;
            tenant.value.limits = limits(input);
            store.save(tx, &mut tenant, "active", None, now).await?;
            tenant.value.policy_revision
        }
    };
    Ok(TenantPolicy {
        tenant_id: input.tenant_id.clone(),
        revision,
        limits: input.limits.clone(),
    })
}

/// Caller holds the head fence. Existing provider reservations survive changes.
pub(in crate::gateway::shared) async fn put_resource(
    authority: &Authority,
    tx: &DatabaseTransaction,
    input: &ResourcePut,
) -> Result<ResourcePolicy> {
    validate_resource(input)?;
    let store = authority.store();
    let now = Store::now(tx).await?;
    let revision = match input.expected_revision {
        None => {
            ensure!(
                store
                    .create(
                        tx,
                        NewRecord {
                            kind: Kind::Resource,
                            key: &input.resource_pool_id,
                            status: "active",
                            due_at_ms: None,
                            now_ms: now,
                            value: &Resource {
                                concurrent_attempts: input.concurrent_attempts,
                                policy_revision: 1,
                                active: 0,
                            },
                        }
                    )
                    .await?,
                Rejection::AuthorityChanged
            );
            1
        }
        Some(expected) => {
            let mut resource: Stored<Resource> = store
                .lock_optional(tx, Kind::Resource, &input.resource_pool_id)
                .await?
                .ok_or(Rejection::NotFound)?;
            ensure!(
                resource.value.policy_revision == expected,
                Rejection::AuthorityChanged
            );
            resource.value.policy_revision = expected
                .checked_add(1)
                .context("resource policy revision exhausted")?;
            resource.value.concurrent_attempts = input.concurrent_attempts;
            store.save(tx, &mut resource, "active", None, now).await?;
            resource.value.policy_revision
        }
    };
    Ok(ResourcePolicy {
        resource_pool_id: input.resource_pool_id.clone(),
        revision,
        concurrent_attempts: input.concurrent_attempts,
    })
}

pub(in crate::gateway::shared) async fn tenant(
    authority: &Authority,
    tx: &DatabaseTransaction,
    id: &str,
) -> Result<TenantPolicy> {
    validate_id(id)?;
    let row: Stored<Tenant> = authority
        .store()
        .read(tx, Kind::Tenant, id)
        .await?
        .ok_or(Rejection::NotFound)?;
    Ok(TenantPolicy {
        tenant_id: id.into(),
        revision: row.value.policy_revision,
        limits: bitrouter_ai::gateway::operator::TenantLimits {
            concurrent_executions: row.value.limits.concurrent_executions,
            requests_per_minute: row.value.limits.requests_per_minute,
            resource_pools: row.value.limits.resource_pools,
        },
    })
}

pub(in crate::gateway::shared) async fn resource(
    authority: &Authority,
    tx: &DatabaseTransaction,
    id: &str,
) -> Result<ResourcePolicy> {
    validate_id(id)?;
    let row: Stored<Resource> = authority
        .store()
        .read(tx, Kind::Resource, id)
        .await?
        .ok_or(Rejection::NotFound)?;
    Ok(ResourcePolicy {
        resource_pool_id: id.into(),
        revision: row.value.policy_revision,
        concurrent_attempts: row.value.concurrent_attempts,
    })
}

pub(in crate::gateway::shared) async fn execution(
    authority: &Authority,
    tx: &DatabaseTransaction,
    id: &str,
) -> Result<bitrouter_ai::gateway::execution::GatewayExecution> {
    validate_id(id)?;
    let row: Stored<super::Execution> = authority
        .store()
        .read(tx, Kind::Execution, id)
        .await?
        .ok_or(Rejection::NotFound)?;
    let revision = row.revision()?;
    let mut view = row.value.view;
    view.revision = revision;
    Ok(view)
}
