//! Read-only bounded catalog checks, before the original typed decoders run.
//! SQLite metadata API: <https://www.sqlite.org/pragma.html#pragma_table_xinfo>
use super::{
    super::{format::Inventory, schema},
    plan,
};
use anyhow::{Result, ensure};
use futures::TryStreamExt;
use sea_orm::sqlx::{Row, SqliteConnection};
use std::collections::{BTreeMap, BTreeSet};

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Do not discover an oversized fresh target by first materializing its payload.
pub(super) async fn preflight(db: &mut SqliteConnection) -> Result<()> {
    let mut stream = sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(type AS BLOB))<=32 THEN type ELSE NULL END AS type, CASE WHEN length(CAST(sql AS BLOB))<=1048576 THEN sql ELSE NULL END AS sql, CASE WHEN sql IS NOT NULL AND length(CAST(sql AS BLOB))>1048576 THEN 1 ELSE 0 END AS oversized FROM sqlite_schema LIMIT 2049").fetch(&mut *db);
    let mut schemas = 0;
    let mut schema_bytes = 0_usize;
    while let Some(row) = stream.try_next().await? {
        schemas += 1;
        ensure!(schemas <= 2048, "target schema bound");
        let kind: String = row.try_get("type")?;
        ensure!(
            row.try_get::<i64, _>("oversized")? == 0,
            "oversized target schema expression"
        );
        let sql: Option<String> = row.try_get("sql")?;
        schema_bytes = schema_bytes
            .checked_add(sql.as_ref().map_or(0, String::len))
            .ok_or_else(|| anyhow::anyhow!("schema byte overflow"))?;
        ensure!(
            schema_bytes <= plan::TARGET_BYTES as usize,
            "target schema byte bound"
        );
        schema::validate(kind.as_bytes(), sql.as_deref().map(str::as_bytes))?;
    }
    drop(stream);
    let tables = sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name, type FROM pragma_table_list WHERE schema='main' AND name!='sqlite_schema' LIMIT 257").fetch_all(&mut *db).await?;
    ensure!(tables.len() <= 256, "target table bound");
    let mut total_rows = 0_u64;
    let mut total_bytes = 0_u64;
    for table in tables {
        ensure!(
            table.try_get::<String, _>("type")? == "table",
            "target view or virtual table"
        );
        let name: String = table.try_get("name")?;
        let columns = sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(name AS BLOB))<=256 THEN name ELSE NULL END AS name,hidden FROM pragma_table_xinfo(?) LIMIT 129").bind(&name).fetch_all(&mut *db).await?;
        ensure!(
            !columns.is_empty() && columns.len() <= 128,
            "target column bound"
        );
        let mut lengths = Vec::new();
        for col in columns {
            ensure!(
                col.try_get::<i64, _>("hidden")? == 0,
                "target hidden/generated column"
            );
            let col = quoted(&col.try_get::<String, _>("name")?);
            lengths.push(format!("coalesce(length(CAST({col} AS BLOB)),0)"));
        }
        let query = format!(
            "SELECT count(*),coalesce(sum({}),0),coalesce(max({}),0) FROM {}",
            lengths.join("+"),
            lengths.join("+"),
            quoted(&name)
        );
        let row = sea_orm::sqlx::query(&query).fetch_one(&mut *db).await?;
        total_rows = total_rows
            .checked_add(u64::try_from(row.try_get::<i64, _>(0)?)?)
            .ok_or_else(|| anyhow::anyhow!("target rows overflow"))?;
        total_bytes = total_bytes
            .checked_add(u64::try_from(row.try_get::<i64, _>(1)?)?)
            .ok_or_else(|| anyhow::anyhow!("target bytes overflow"))?;
        ensure!(
            total_rows <= plan::TARGET_ROWS
                && total_bytes <= plan::TARGET_BYTES
                && row.try_get::<i64, _>(2)? <= super::super::format::MAX_ROW as i64,
            "fresh target exceeds bounded profile"
        );
    }
    Ok(())
}

pub(super) fn dispositions(plan: &plan::Source, inventory: &Inventory) -> Result<()> {
    ensure!(
        inventory.encoding == "UTF-8"
            && inventory.installation_id.as_deref() == Some(plan.installation_id.as_str())
            && plan.tables.len() == inventory.tables.len(),
        "source inventory identity or coverage mismatch"
    );
    let expected = plan
        .tables
        .iter()
        .map(|t| (t.table.as_str(), t))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        expected.len() == inventory.tables.len(),
        "source duplicate disposition"
    );
    for table in &inventory.tables {
        let selected = expected
            .get(table.name.as_str())
            .ok_or_else(|| anyhow::anyhow!("source table not archived/quarantined"))?;
        ensure!(
            selected.rows == table.rows
                && selected.value_bytes == table.value_bytes
                && selected.digest == table.digest,
            "source disposition does not match measured bytes"
        );
    }
    Ok(())
}

pub(super) async fn fresh(
    db: &mut SqliteConnection,
    inventory: &Inventory,
    target: &plan::Target,
) -> Result<()> {
    ensure!(
        inventory.encoding == "UTF-8"
            && inventory.installation_id.as_deref() == Some(target.installation_id.as_str())
            && inventory.rows <= plan::TARGET_ROWS
            && inventory.value_bytes <= plan::TARGET_BYTES,
        "target identity/profile mismatch"
    );
    use sea_orm_migration::MigratorTrait;
    let actual = inventory
        .ledger
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("target ledger missing"))?;
    let expected = crate::db::migration::Migrator::migrations();
    ensure!(
        actual.len() == expected.len()
            && actual
                .iter()
                .zip(&expected)
                .all(|(a, e)| a.version == e.name()),
        "target schema ledger differs from this verifier"
    );
    for table in &inventory.tables {
        let permitted = matches!(
            table.name.as_str(),
            "seaql_migrations"
                | "sqlite_sequence"
                | "bro_runtime_ownership"
                | "tenancy_continuation_epoch"
                | "tenancy_installation"
                | "tenancy_tenants"
                | "tenancy_principals"
                | "tenancy_bindings"
                | "tenancy_credentials"
                | "tenancy_operations"
                | "tenancy_resource_revisions"
                | "tenancy_configurations"
                | "tenancy_active_configurations"
        );
        ensure!(
            permitted || table.rows == 0,
            "target contains operational or imported history"
        );
    }
    let row = sea_orm::sqlx::query(
        "SELECT id,generation,server_instance_id,stopped_at_ms FROM bro_runtime_ownership LIMIT 2",
    )
    .fetch_all(&mut *db)
    .await?;
    ensure!(
        row.len() == 1
            && row[0].try_get::<i64, _>("id")? == 1
            && row[0].try_get::<i64, _>("generation")? == 0
            && row[0]
                .try_get::<Option<String>, _>("server_instance_id")?
                .is_none()
            && row[0].try_get::<Option<i64>, _>("stopped_at_ms")?.is_none(),
        "target runtime history is not fresh"
    );
    if inventory.tables.iter().any(|t| t.name == "sqlite_sequence") {
        let used: i64 =
            sea_orm::sqlx::query_scalar("SELECT count(*) FROM sqlite_sequence WHERE seq<>0")
                .fetch_one(&mut *db)
                .await?;
        ensure!(used == 0, "target deleted autoincrement history");
    }
    // Schema grammar alone does not prove exact identities. Validate actual PK
    // order and every stored key comparison before trusting typed authority reads.
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
        let columns = sea_orm::sqlx::query(
            "SELECT name,pk FROM pragma_table_xinfo(?) WHERE pk>0 ORDER BY pk LIMIT 7",
        )
        .bind(table)
        .fetch_all(&mut *db)
        .await?;
        ensure!(columns.len() == keys.len(), "target identity key mismatch");
        for (column, key) in columns.iter().zip(keys) {
            ensure!(
                column.try_get::<String, _>("name")? == *key,
                "target identity key order mismatch"
            );
        }
        let indexes =
            sea_orm::sqlx::query("SELECT name,partial FROM pragma_index_list(?) LIMIT 33")
                .bind(table)
                .fetch_all(&mut *db)
                .await?;
        ensure!(indexes.len() <= 32, "target index bound");
        for index in indexes {
            ensure!(
                index.try_get::<i64, _>("partial")? == 0,
                "target partial index"
            );
            let name: String = index.try_get("name")?;
            let columns = sea_orm::sqlx::query(
                "SELECT coll,cid FROM pragma_index_xinfo(?) WHERE key=1 LIMIT 129",
            )
            .bind(name)
            .fetch_all(&mut *db)
            .await?;
            ensure!(columns.len() <= 128, "target index columns bound");
            for col in columns {
                ensure!(
                    col.try_get::<i64, _>("cid")? >= 0
                        && col.try_get::<String, _>("coll")? == "BINARY",
                    "target key comparison is not exact"
                );
            }
        }
    }
    Ok(())
}

pub(super) async fn credential_hashes(db: &mut SqliteConnection) -> Result<BTreeSet<String>> {
    let rows = sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(key_hash AS BLOB))=64 THEN key_hash ELSE NULL END AS hash FROM tenancy_credentials LIMIT 257").fetch_all(&mut *db).await?;
    ensure!(
        !rows.is_empty() && rows.len() <= 256,
        "fresh credential count"
    );
    let mut hashes = BTreeSet::new();
    for row in rows {
        let hash: String = row.try_get("hash")?;
        super::super::files::validate_digest(&hash)?;
        ensure!(hashes.insert(hash), "duplicate target credential hash");
    }
    Ok(hashes)
}
pub(super) async fn no_copied_credentials(
    db: &mut SqliteConnection,
    hashes: &BTreeSet<String>,
) -> Result<()> {
    // Include expired/revoked credentials, which Snapshot intentionally excludes.
    let mut stream = sea_orm::sqlx::query("SELECT CASE WHEN length(CAST(key_hash AS BLOB))=64 THEN key_hash ELSE NULL END AS hash FROM tenancy_credentials LIMIT 1000001").fetch(&mut *db);
    let mut rows = 0_u64;
    while let Some(row) = stream.try_next().await? {
        rows += 1;
        ensure!(
            rows <= super::super::format::MAX_ROWS,
            "source credential bound"
        );
        let hash: String = row.try_get("hash")?;
        super::super::files::validate_digest(&hash)?;
        ensure!(
            !hashes.contains(&hash),
            "old ingress credential copied to target"
        );
    }
    Ok(())
}
