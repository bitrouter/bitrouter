//! Fresh-only catalog rules shared only where the two consumers agree.
use crate::offline::format::Inventory;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::identity::InstallationId;
use sea_orm::{ConnectionTrait, Statement};

#[cfg(test)]
mod tests;

const AUTHORITY: &[&str] = &[
    "tenancy_installation",
    "tenancy_tenants",
    "tenancy_principals",
    "tenancy_bindings",
    "tenancy_credentials",
    "tenancy_operations",
];

pub(crate) fn count(inventory: &Inventory, table: &str) -> Result<u64> {
    Ok(inventory
        .tables
        .iter()
        .find(|entry| entry.name == table)
        .context("required table missing")?
        .rows)
}

pub(crate) async fn recovery(
    db: &impl ConnectionTrait,
    inventory: &Inventory,
    installation: &InstallationId,
) -> Result<()> {
    common(db, inventory, installation).await?;
    for table in &inventory.tables {
        ensure!(
            AUTHORITY.contains(&table.name.as_str())
                || [
                    "seaql_migrations",
                    "sqlite_sequence",
                    "bro_runtime_ownership",
                    "tenancy_continuation_epoch",
                    "tenancy_resource_revisions",
                    "tenancy_configurations",
                    "tenancy_active_configurations"
                ]
                .contains(&table.name.as_str())
                || table.rows == 0,
            "target contains operational or imported history"
        );
    }
    Ok(())
}

#[cfg(feature = "core")]
pub(crate) async fn eval(
    db: &impl ConnectionTrait,
    inventory: &Inventory,
    installation: &InstallationId,
) -> Result<()> {
    common(db, inventory, installation).await?;
    for table in &inventory.tables {
        ensure!(
            AUTHORITY.contains(&table.name.as_str())
                || [
                    "seaql_migrations",
                    "sqlite_sequence",
                    "bro_runtime_ownership",
                    "provider_continuation_key_epoch"
                ]
                .contains(&table.name.as_str())
                || table.rows == 0,
            "Eval target contains configuration/resource/work history"
        );
    }
    Ok(())
}

async fn common(
    db: &impl ConnectionTrait,
    inventory: &Inventory,
    installation: &InstallationId,
) -> Result<()> {
    use sea_orm_migration::MigratorTrait;
    ensure!(
        inventory.encoding == "UTF-8"
            && inventory.installation_id.as_deref() == Some(installation.as_str()),
        "fresh target identity mismatch"
    );
    let ledger = inventory.ledger.as_ref().context("target ledger missing")?;
    let expected = crate::db::migration::Migrator::migrations();
    ensure!(
        ledger.len() == expected.len()
            && ledger
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.version == expected.name()),
        "target ledger differs from verifier"
    );
    let rows = db.query_all(Statement::from_string(db.get_database_backend(), "SELECT CASE WHEN typeof(id)='integer' AND id=1 AND typeof(generation)='integer' AND generation=0 AND server_instance_id IS NULL AND stopped_at_ms IS NULL THEN 1 ELSE 0 END AS fresh FROM bro_runtime_ownership LIMIT 2")).await?;
    ensure!(
        rows.len() == 1 && rows[0].try_get::<i64>("", "fresh")? == 1,
        "target runtime history is not fresh"
    );
    if inventory
        .tables
        .iter()
        .any(|table| table.name == "sqlite_sequence")
    {
        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT count(*) AS count FROM sqlite_sequence WHERE seq<>0",
            ))
            .await?
            .context("sequence count")?;
        ensure!(
            row.try_get::<i64>("", "count")? == 0,
            "target deleted sequence history"
        );
    }
    // Full authority validation qualifies retained keys. Additionally retain the
    // original fresh verifier's exact binary PK/index comparison contract.
    for (table, keys) in [
        ("tenancy_installation", &["singleton"][..]),
        ("tenancy_tenants", &["id"][..]),
        ("tenancy_principals", &["id"][..]),
        ("tenancy_bindings", &["tenant_id", "id"][..]),
        ("tenancy_credentials", &["id"][..]),
        (
            "tenancy_operations",
            &["installation_id", "principal_id", "operation_id"][..],
        ),
        (
            "tenancy_resource_revisions",
            &[
                "owner_kind",
                "owner_id",
                "resource_kind",
                "resource_id",
                "revision_id",
            ][..],
        ),
        ("tenancy_configurations", &["tenant_id", "revision_id"][..]),
        ("tenancy_active_configurations", &["tenant_id"][..]),
    ] {
        let columns = db
            .query_all(Statement::from_sql_and_values(
                db.get_database_backend(),
                "SELECT name,pk FROM pragma_table_xinfo(?) WHERE pk>0 ORDER BY pk LIMIT 7",
                [table.into()],
            ))
            .await?;
        ensure!(columns.len() == keys.len(), "target identity key mismatch");
        for (column, key) in columns.iter().zip(keys) {
            ensure!(
                column.try_get::<String>("", "name")? == *key,
                "target PK order mismatch"
            );
        }
        let indexes = db
            .query_all(Statement::from_sql_and_values(
                db.get_database_backend(),
                "SELECT name,partial FROM pragma_index_list(?) LIMIT 33",
                [table.into()],
            ))
            .await?;
        ensure!(indexes.len() <= 32, "target schema index bound");
        for index in indexes {
            ensure!(
                index.try_get::<i64>("", "partial")? == 0,
                "target partial index"
            );
            let fields = db
                .query_all(Statement::from_sql_and_values(
                    db.get_database_backend(),
                    "SELECT coll,cid FROM pragma_index_xinfo(?) WHERE key=1 LIMIT 129",
                    [index.try_get::<String>("", "name")?.into()],
                ))
                .await?;
            ensure!(fields.len() <= 128, "target schema index width");
            for field in fields {
                ensure!(
                    field.try_get::<i64>("", "cid")? >= 0
                        && field.try_get::<String>("", "coll")? == "BINARY",
                    "target key comparison is not exact"
                );
            }
        }
    }
    Ok(())
}
