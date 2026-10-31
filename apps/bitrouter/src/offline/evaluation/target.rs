//! Inspect already provisioned target facts; never initialize or grant authority.
use super::*;
use crate::core::eval::store::migration::Assignment;
use anyhow::Context;
use bitrouter_ai::tenancy::{
    authority::{GrantScope, TenantRole, TenantState},
    identity::{PrincipalId, TenantId},
    validation::Validate,
};
use sea_orm::sqlx::{Row, SqliteConnection};
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn validate(
    db: &mut SqliteConnection,
    inventory: &super::super::format::Inventory,
    installation: &InstallationId,
    assignments: &BTreeMap<String, Assignment>,
    operations: &[String],
) -> Result<()> {
    ensure!(
        inventory.installation_id.as_deref() == Some(installation.as_str()),
        "target installation mismatch"
    );
    ensure!(
        inventory.rows <= 4096 && inventory.value_bytes <= 32 * 1024 * 1024,
        "fresh provisioning inventory bound"
    );
    // This first consumer accepts only authority provisioning, before any static
    // configuration or work. No copied old credential history can become fresh.
    let authority = [
        "tenancy_installation",
        "tenancy_tenants",
        "tenancy_principals",
        "tenancy_bindings",
        "tenancy_credentials",
        "tenancy_operations",
    ];
    for table in &inventory.tables {
        let permitted = authority.contains(&table.name.as_str())
            || matches!(
                table.name.as_str(),
                "seaql_migrations"
                    | "sqlite_sequence"
                    | "bro_runtime_ownership"
                    | "provider_continuation_key_epoch"
            );
        ensure!(
            permitted || table.rows == 0,
            "destination has non-provisioning history"
        );
        ensure!(
            !authority.contains(&table.name.as_str())
                || (table.rows <= 4096 && table.value_bytes <= 32 * 1024 * 1024),
            "authority bounds"
        );
    }
    let runtime = sea_orm::sqlx::query(
        "SELECT id,generation,server_instance_id,stopped_at_ms FROM bro_runtime_ownership LIMIT 2",
    )
    .fetch_all(&mut *db)
    .await?;
    ensure!(
        runtime.len() == 1
            && runtime[0].try_get::<i64, _>("id")? == 1
            && runtime[0].try_get::<i64, _>("generation")? == 0
            && runtime[0]
                .try_get::<Option<String>, _>("server_instance_id")?
                .is_none()
            && runtime[0]
                .try_get::<Option<i64>, _>("stopped_at_ms")?
                .is_none(),
        "target has runtime ownership history"
    );
    use sea_orm_migration::MigratorTrait;
    let expected = crate::db::migration::Migrator::migrations()
        .into_iter()
        .map(|migration| migration.name().to_owned())
        .collect::<BTreeSet<_>>();
    let ledger = inventory
        .ledger
        .as_ref()
        .context("target migration ledger missing")?;
    let actual = ledger
        .iter()
        .map(|entry| entry.version.clone())
        .collect::<BTreeSet<_>>();
    ensure!(
        actual.len() == ledger.len() && actual == expected,
        "current target migration ledger required"
    );
    let row=sea_orm::sqlx::query("SELECT singleton,schema_version,operation_ids,safety_operation_ids,saturated,authority_revision FROM tenancy_installation LIMIT 2").fetch_all(&mut *db).await?;
    ensure!(row.len() == 1, "target identity cardinality");
    let row = &row[0];
    ensure!(
        row.try_get::<i64, _>("singleton")? == 1
            && row.try_get::<i64, _>("schema_version")? == 1
            && row.try_get::<i64, _>("operation_ids")? == operations.len() as i64
            && row.try_get::<i64, _>("safety_operation_ids")? == 0
            && !row.try_get::<bool, _>("saturated")?
            && row.try_get::<i64, _>("authority_revision")? == operations.len() as i64,
        "fresh authority counters mismatch"
    );
    let actual=sea_orm::sqlx::query("SELECT installation_id,operation_id,safety,result_json FROM tenancy_operations ORDER BY operation_id COLLATE BINARY LIMIT 4097").fetch_all(&mut *db).await?;
    let declared = operations.iter().collect::<BTreeSet<_>>();
    ensure!(
        declared.len() == operations.len()
            && !operations.is_empty()
            && actual.len() == operations.len(),
        "provisioning history mismatch"
    );
    let mut seen = BTreeSet::new();
    let mut revisions = BTreeSet::new();
    for row in actual {
        let id: String = row.try_get("operation_id")?;
        ensure!(
            row.try_get::<String, _>("installation_id")? == installation.as_str()
                && !row.try_get::<bool, _>("safety")?
                && declared.contains(&id)
                && seen.insert(id.clone()),
            "foreign provisioning operation"
        );
        let result: bitrouter_ai::tenancy::operation::OperationResult =
            serde_json::from_str(&row.try_get::<String, _>("result_json")?)?;
        result.validate()?;
        let revision = result
            .authority_revision
            .ok_or_else(|| anyhow::anyhow!("unpublished provisioning operation"))?
            .get();
        ensure!(
            result.operation_id.as_str() == id
                && result.status == bitrouter_ai::tenancy::operation::OperationStatus::Applied
                && revision > 0
                && revision <= operations.len() as u64
                && revisions.insert(revision),
            "non-fresh provisioning result"
        );
    }
    let mut tenants = BTreeSet::new();
    for row in sea_orm::sqlx::query("SELECT id,state FROM tenancy_tenants LIMIT 33")
        .fetch_all(&mut *db)
        .await?
    {
        let id: TenantId = row.try_get::<String, _>("id")?.try_into()?;
        let state: TenantState =
            serde_json::from_value(serde_json::Value::String(row.try_get("state")?))?;
        ensure!(
            state == TenantState::Active && tenants.insert(id),
            "target tenant state"
        );
    }
    ensure!(tenants.len() <= 32, "tenant bound");
    let mut principals = BTreeSet::new();
    for row in sea_orm::sqlx::query("SELECT id FROM tenancy_principals LIMIT 129")
        .fetch_all(&mut *db)
        .await?
    {
        ensure!(
            principals.insert(PrincipalId::try_from(row.try_get::<String, _>("id")?)?),
            "principal duplicate"
        );
    }
    ensure!(principals.len() <= 128, "principal bound");
    let bindings = sea_orm::sqlx::query(
        "SELECT tenant_id,principal_id,role,scope_json,expires_at FROM tenancy_bindings LIMIT 257",
    )
    .fetch_all(&mut *db)
    .await?;
    ensure!(bindings.len() <= 256, "binding bound");
    let mut qualified = BTreeSet::new();
    for row in bindings {
        let tenant: TenantId = row.try_get::<String, _>("tenant_id")?.try_into()?;
        let principal: PrincipalId = row.try_get::<String, _>("principal_id")?.try_into()?;
        ensure!(
            tenants.contains(&tenant) && principals.contains(&principal),
            "orphan target binding"
        );
        let role: TenantRole =
            serde_json::from_value(serde_json::Value::String(row.try_get("role")?))?;
        let scope: GrantScope = serde_json::from_str(&row.try_get::<String, _>("scope_json")?)?;
        scope.validate()?;
        let expiry =
            chrono::DateTime::parse_from_rfc3339(&row.try_get::<String, _>("expires_at")?)?;
        if role == TenantRole::TenantAdmin
            && matches!(scope, GrantScope::Tenant { .. })
            && expiry > chrono::Utc::now()
        {
            qualified.insert((tenant, principal));
        }
    }
    for assignment in assignments.values() {
        ensure!(
            qualified.contains(&(assignment.tenant.clone(), assignment.principal.clone())),
            "mapping lacks current tenant-wide admin binding"
        );
    }
    // Never reuse a deleted ledger's autoincrement high-water mark.
    if inventory.tables.iter().any(|t| t.name == "sqlite_sequence") {
        let used: i64 =
            sea_orm::sqlx::query_scalar("SELECT count(*) FROM sqlite_sequence WHERE seq<>0")
                .fetch_one(&mut *db)
                .await?;
        ensure!(used == 0, "destination has prior Eval sequence history");
    }
    Ok(())
}
