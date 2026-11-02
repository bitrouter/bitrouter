//! Bounded local lifecycle metadata, separate from immutable resource contents.
//! The single authority writer owns transaction, revision and publication fences.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::ResourceKey,
    configuration::ResourceRef,
    identity::{AuthorityRevision, InstallationId, RevisionId},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, QueryResult, Value,
    sea_query::{Alias, Expr, Func, OnConflict, Query, SelectStatement},
};

use crate::tenancy::configuration_store;

const TABLE: &str = "tenancy_resource_lifecycle";
const KEY: [&str; 5] = [
    "owner_kind",
    "owner_id",
    "resource_kind",
    "resource_id",
    "revision_id",
];
pub(in crate::tenancy) const MAX_RECORDS: usize = 4096;

/// Nonsecret status for one exact immutable resource revision.
#[derive(Clone)]
pub(in crate::tenancy) struct Record {
    pub reference: ResourceRef,
    pub is_head: bool,
    pub revoked: bool,
    pub authority_revision: AuthorityRevision,
}

/// Lifecycle snapshot; absence means unmanaged and not explicitly revoked.
#[derive(Clone, Default)]
pub(in crate::tenancy) struct State {
    records: HashMap<ResourceRef, Record>,
}

impl State {
    pub(in crate::tenancy) fn head(&self, key: &ResourceKey) -> Option<&RevisionId> {
        self.records.values().find_map(|record| {
            let reference = &record.reference;
            (record.is_head
                && reference.owner_kind == key.owner_kind
                && reference.owner_id == key.owner_id
                && reference.resource_kind == key.resource_kind
                && reference.resource_id == key.resource_id)
                .then_some(&reference.revision_id)
        })
    }

    pub(in crate::tenancy) fn revoked(&self, reference: &ResourceRef) -> bool {
        self.record(reference).is_some_and(|record| record.revoked)
    }

    pub(in crate::tenancy) fn record(&self, reference: &ResourceRef) -> Option<&Record> {
        self.records.get(reference)
    }

    /// Insert caller-validated metadata after persistence; never evict history.
    /// Callers check capacity before adding a reference and demote the old head.
    pub(in crate::tenancy) fn insert(&mut self, record: Record) -> Option<Record> {
        self.records.insert(record.reference.clone(), record)
    }

    pub(in crate::tenancy) fn len(&self) -> usize {
        self.records.len()
    }
}

fn select(backend: DatabaseBackend) -> SelectStatement {
    let mut query = Query::select();
    query
        .from(Alias::new(TABLE))
        .limit((MAX_RECORDS + 1) as u64);
    for (name, limit) in [
        ("installation_id", 128_u64),
        ("owner_kind", 32),
        ("owner_id", 128),
        ("resource_kind", 32),
        ("resource_id", 128),
        ("revision_id", 128),
    ] {
        // Project NULL, never filter corruption away or allocate an unbounded cell.
        // SQLite length(TEXT) counts characters, so measure the BLOB's bytes.
        // https://sqlite.org/lang_corefunc.html#length
        // https://www.postgresql.org/docs/current/functions-string.html
        // https://dev.mysql.com/doc/refman/8.4/en/string-functions.html#function_octet-length
        let column = Expr::col(Alias::new(name));
        let length = if backend == DatabaseBackend::Sqlite {
            Func::cust(Alias::new("length")).arg(column.clone().cast_as(Alias::new("BLOB")))
        } else {
            Func::cust(Alias::new("octet_length")).arg(column.clone())
        };
        let mut valid = Expr::expr(length).lte(limit);
        if backend == DatabaseBackend::Sqlite {
            valid = valid
                .and(Expr::expr(Func::cust(Alias::new("typeof")).arg(column.clone())).eq("text"));
        }
        query.expr_as(
            Expr::case(valid, column).finally(Expr::val(Option::<String>::None)),
            Alias::new(name),
        );
    }
    for name in ["is_head", "revoked", "authority_revision"] {
        let column = Expr::col(Alias::new(name));
        let mut valid = if name == "authority_revision" {
            column.clone().between(0_i64, AuthorityRevision::MAX as i64)
        } else {
            column.clone().is_in([false, true])
        };
        // SQLite's INTEGER/BOOLEAN affinity also accepts arbitrary TEXT/BLOB.
        // https://sqlite.org/datatype3.html
        if backend == DatabaseBackend::Sqlite {
            valid = valid.and(
                Expr::expr(Func::cust(Alias::new("typeof")).arg(column.clone())).eq("integer"),
            );
        }
        query.expr_as(
            Expr::case(valid, column).finally(Expr::cust("NULL")),
            Alias::new(name),
        );
    }
    query
}

fn text(row: &QueryResult, name: &str) -> Result<String> {
    row.try_get::<Option<String>>("", name)?
        .context("invalid or oversized resource lifecycle field")
}

async fn records(db: &impl ConnectionTrait, installation: &InstallationId) -> Result<Vec<Record>> {
    let backend = db.get_database_backend();
    let rows = db.query_all(backend.build(&select(backend))).await?;
    ensure!(
        rows.len() <= MAX_RECORDS,
        "resource lifecycle capacity exceeded"
    );
    rows.into_iter()
        .map(|row| {
            ensure!(
                text(&row, "installation_id")? == installation.as_str(),
                "resource lifecycle installation mismatch"
            );
            Ok(Record {
                reference: ResourceRef {
                    owner_kind: serde_json::from_value(text(&row, "owner_kind")?.into())?,
                    owner_id: text(&row, "owner_id")?.try_into()?,
                    resource_kind: serde_json::from_value(text(&row, "resource_kind")?.into())?,
                    resource_id: text(&row, "resource_id")?.try_into()?,
                    revision_id: text(&row, "revision_id")?.try_into()?,
                },
                is_head: row.try_get("", "is_head")?,
                revoked: row.try_get("", "revoked")?,
                authority_revision: u64::try_from(row.try_get::<i64>("", "authority_revision")?)?
                    .try_into()?,
            })
        })
        .collect()
}

/// Load all lifecycle metadata against the caller's installation and snapshot.
///
/// # Errors
/// Rejects corruption, capacity overflow, duplicate heads, future authority and
/// references without a valid immutable resource revision. No contents are exposed.
pub(in crate::tenancy) async fn load(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    authority: AuthorityRevision,
) -> Result<State> {
    let mut state = State::default();
    let mut heads = HashSet::new();
    for record in records(db, installation).await? {
        ensure!(
            record.authority_revision <= authority,
            "resource lifecycle authority mismatch"
        );
        ensure!(
            !record.is_head || heads.insert(record.reference.key()),
            "duplicate resource lifecycle head"
        );
        ensure!(
            configuration_store::resource(db, &record.reference)
                .await?
                .is_some(),
            "resource lifecycle revision missing"
        );
        ensure!(
            state.insert(record).is_none(),
            "duplicate resource lifecycle revision"
        );
    }
    Ok(state)
}

/// Upsert exactly one revision inside the single writer's caller-owned transaction.
/// Rotation demotes the old head before inserting the new one; revocation supplies
/// the exact revision's existing head flag. Immutable contents are never mutated.
///
/// # Errors
/// Rejects invalid retained metadata, installation mismatch, capacity overflow,
/// conflicting heads or database errors. The caller must roll back on any error.
pub(in crate::tenancy) async fn write(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    record: &Record,
) -> Result<()> {
    let retained = records(db, installation).await?;
    ensure!(
        retained.len() < MAX_RECORDS || retained.iter().any(|r| r.reference == record.reference),
        "resource lifecycle capacity exceeded"
    );
    ensure!(
        !record.is_head
            || !retained.iter().any(|r| {
                r.is_head
                    && r.reference != record.reference
                    && r.reference.key() == record.reference.key()
            }),
        "duplicate resource lifecycle head"
    );
    let reference = &record.reference;
    let enum_text = |value: serde_json::Value| -> Result<Value> {
        Ok(value
            .as_str()
            .context("invalid resource lifecycle identity")?
            .into())
    };
    let fields: [(&str, Value); 9] = [
        (
            "owner_kind",
            enum_text(serde_json::to_value(reference.owner_kind)?)?,
        ),
        ("owner_id", reference.owner_id.as_str().into()),
        (
            "resource_kind",
            enum_text(serde_json::to_value(reference.resource_kind)?)?,
        ),
        ("resource_id", reference.resource_id.as_str().into()),
        ("revision_id", reference.revision_id.as_str().into()),
        ("installation_id", installation.as_str().into()),
        ("is_head", record.is_head.into()),
        ("revoked", record.revoked.into()),
        (
            "authority_revision",
            i64::try_from(record.authority_revision.get())?.into(),
        ),
    ];
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new(TABLE))
        .columns(fields.iter().map(|(name, _)| Alias::new(*name)))
        .values(fields.into_iter().map(|(_, value)| value.into()))?
        .on_conflict(
            OnConflict::columns(KEY.map(Alias::new))
                .update_columns(["is_head", "revoked", "authority_revision"].map(Alias::new))
                .to_owned(),
        );
    db.execute(db.get_database_backend().build(&insert)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKind};
    use sea_orm::{DatabaseConnection, Statement, TransactionTrait};
    use sea_orm_migration::{MigratorTrait, SchemaManager};
    use serde_json::json;

    async fn database() -> Result<DatabaseConnection> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        for migration in crate::db::migration::Migrator::migrations() {
            if [
                "m20240101_000028_",
                "m20240101_000029_",
                "m20240101_000039_",
            ]
            .iter()
            .any(|prefix| migration.name().starts_with(prefix))
            {
                migration.up(&manager).await?;
            }
        }
        Ok(db)
    }

    fn record(revision: &str) -> Result<Record> {
        Ok(Record {
            reference: ResourceRef {
                owner_kind: OwnerKind::Tenant,
                owner_id: "tenant-a".try_into()?,
                resource_kind: ResourceKind::Credential,
                resource_id: "resource-a".try_into()?,
                revision_id: revision.try_into()?,
            },
            is_head: true,
            revoked: false,
            authority_revision: 1.try_into()?,
        })
    }

    async fn retain(db: &impl ConnectionTrait, record: &Record) -> Result<()> {
        configuration_store::insert_resource(
            db,
            &configuration_store::StoredResource {
                reference: record.reference.clone(),
                compiler_version: "fixture-v1".into(),
                payload: json!({"ciphertext": "sealed-fixture"}),
                dependencies: json!([]),
                content_digest: "a".repeat(64).try_into()?,
                created_at: "2026-01-01T00:00:00Z".try_into()?,
            },
        )
        .await
    }

    async fn sql(db: &impl ConnectionTrait, sql: &str) -> Result<()> {
        db.execute(Statement::from_string(DatabaseBackend::Sqlite, sql))
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn rotations_and_exact_revocations_reload_and_rollback() -> Result<()> {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let mut old = record("v1")?;
        let mut new = record("v2")?;
        let key = old.reference.key();
        let empty = load(&db, &installation, 0.try_into()?).await?;
        ensure!(empty.len() == 0 && empty.head(&key).is_none());
        ensure!(!empty.revoked(&old.reference) && empty.record(&old.reference).is_none());
        let transaction = db.begin().await?;
        retain(&transaction, &old).await?;
        write(&transaction, &installation, &old).await?;
        transaction.commit().await?;
        old.is_head = false;
        old.authority_revision = 2.try_into()?;
        new.authority_revision = 2.try_into()?;
        let transaction = db.begin().await?;
        retain(&transaction, &new).await?;
        write(&transaction, &installation, &old).await?;
        write(&transaction, &installation, &new).await?;
        transaction.commit().await?;
        let rotated = load(&db, &installation, 2.try_into()?).await?;
        ensure!(rotated.len() == 2 && rotated.head(&key) == Some(&new.reference.revision_id));
        ensure!(!rotated.revoked(&old.reference) && !rotated.revoked(&new.reference));
        let current = new.reference.clone();
        for target in [&mut old, &mut new] {
            target.revoked = true;
            target.authority_revision = 3.try_into()?;
            let transaction = db.begin().await?;
            write(&transaction, &installation, target).await?;
            transaction.commit().await?;
            let loaded = load(&db, &installation, 3.try_into()?).await?;
            ensure!(loaded.revoked(&target.reference));
            ensure!(loaded.revoked(&current) == (target.reference == current));
            ensure!(loaded.head(&key) == Some(&current.revision_id));
        }
        let loaded = load(&db, &installation, 3.try_into()?).await?;
        ensure!(loaded.len() == 2 && loaded.revoked(&old.reference));
        ensure!(
            loaded
                .record(&old.reference)
                .context("missing old revision")?
                .authority_revision
                == 3.try_into()?
        );
        let transaction = db.begin().await?;
        new.is_head = false;
        write(&transaction, &installation, &new).await?;
        transaction.rollback().await?;
        ensure!(
            load(&db, &installation, 3.try_into()?).await?.head(&key)
                == Some(&new.reference.revision_id)
        );
        let immutable = configuration_store::resource(&db, &old.reference)
            .await?
            .context("missing immutable revision")?;
        ensure!(immutable.payload == json!({"ciphertext": "sealed-fixture"}));
        Ok(())
    }

    #[tokio::test]
    async fn namespaces_are_byte_exact_and_revocation_never_crosses_identity() -> Result<()> {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let mut base = record("v1")?;
        base.revoked = true;
        let mut variants = Vec::new();
        for dimension in 0..7 {
            let mut other = record("v1")?;
            match dimension {
                0 => other.reference.owner_kind = OwnerKind::OperatorPool,
                1 => other.reference.owner_id = "tenant-b".try_into()?,
                2 => other.reference.owner_id = "Tenant-a".try_into()?,
                3 => other.reference.resource_kind = ResourceKind::Route,
                4 => other.reference.resource_id = "Resource-a".try_into()?,
                5 => {
                    other.reference.revision_id = "V1".try_into()?;
                    other.is_head = false;
                }
                _ => other.reference.resource_id = "resource-b".try_into()?,
            }
            variants.push(other);
        }
        let transaction = db.begin().await?;
        for item in std::iter::once(&base).chain(&variants) {
            retain(&transaction, item).await?;
            write(&transaction, &installation, item).await?;
        }
        transaction.commit().await?;
        let state = load(&db, &installation, 1.try_into()?).await?;
        ensure!(state.len() == 8 && state.revoked(&base.reference));
        for other in variants {
            ensure!(!state.revoked(&other.reference));
            ensure!(state.record(&other.reference).is_some());
            if other.is_head {
                ensure!(state.head(&other.reference.key()) == Some(&other.reference.revision_id));
            }
        }
        ensure!(
            load(&db, &"Installation".try_into()?, 1.try_into()?)
                .await
                .is_err()
        );
        ensure!(
            write(&db, &"Installation".try_into()?, &base)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_exact_revision_duplicate_heads_and_future_authority_are_rejected() -> Result<()>
    {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let first = record("v1")?;
        retain(&db, &first).await?;
        let mut second = record("v2")?;
        second.is_head = false;
        write(&db, &installation, &second).await?;
        let error = load(&db, &installation, 1.try_into()?)
            .await
            .err()
            .context("missing revision accepted")?;
        ensure!(error.to_string() == "resource lifecycle revision missing");
        retain(&db, &second).await?;
        write(&db, &installation, &first).await?;
        ensure!(load(&db, &installation, 0.try_into()?).await.is_err());
        ensure!(load(&db, &installation, 1.try_into()?).await?.len() == 2);
        second.is_head = true;
        ensure!(write(&db, &installation, &second).await.is_err());
        sql(&db, "UPDATE tenancy_resource_lifecycle SET is_head=1").await?;
        let error = load(&db, &installation, 1.try_into()?)
            .await
            .err()
            .context("duplicate heads accepted")?;
        ensure!(error.to_string() == "duplicate resource lifecycle head");
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_cells_are_null_in_sql_before_driver_allocation() -> Result<()> {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let item = record("v1")?;
        retain(&db, &item).await?;
        write(&db, &installation, &item).await?;
        for name in [
            "installation_id",
            "owner_kind",
            "owner_id",
            "resource_kind",
            "resource_id",
            "revision_id",
            "is_head",
            "revoked",
            "authority_revision",
        ] {
            for value in [
                "zeroblob(1048576)",
                "replace(hex(zeroblob(524288)), '0', 'x')",
            ] {
                let transaction = db.begin().await?;
                sql(
                    &transaction,
                    &format!("UPDATE tenancy_resource_lifecycle SET {name}={value}"),
                )
                .await?;
                let row = transaction
                    .query_one(DatabaseBackend::Sqlite.build(&select(DatabaseBackend::Sqlite)))
                    .await?
                    .context("missing corrupt row")?;
                ensure!(row.try_get::<Option<String>>("", name)?.is_none());
                ensure!(
                    load(&transaction, &installation, 1.try_into()?)
                        .await
                        .is_err()
                );
                transaction.rollback().await?;
            }
        }
        for assignment in [
            "is_head=2",
            "revoked=-1",
            "authority_revision=-1",
            "authority_revision=9007199254740992",
            "authority_revision=1.5",
            "owner_id='invalid owner'",
            "owner_kind='unknown'",
            "resource_kind='unknown'",
            "owner_id=replace(hex(zeroblob(64)), '0', char(233))",
        ] {
            let transaction = db.begin().await?;
            sql(
                &transaction,
                &format!("UPDATE tenancy_resource_lifecycle SET {assignment}"),
            )
            .await?;
            ensure!(
                load(&transaction, &installation, 1.try_into()?)
                    .await
                    .is_err()
            );
            transaction.rollback().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn retained_capacity_allows_exact_updates_but_never_evicts() -> Result<()> {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let mut item = record("v1")?;
        retain(&db, &item).await?;
        write(&db, &installation, &item).await?;
        sql(&db, "WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i+1 FROM n WHERE i<4096) INSERT INTO tenancy_resource_lifecycle SELECT 'tenant','tenant-a','credential','resource-a','v'||i,'installation',0,0,1 FROM n").await?;
        item.revoked = true;
        write(&db, &installation, &item).await?;
        let mut added = record("v4097")?;
        added.is_head = false;
        ensure!(write(&db, &installation, &added).await.is_err());
        ensure!(records(&db, &installation).await?.len() == MAX_RECORDS);
        sql(&db, "INSERT INTO tenancy_resource_lifecycle VALUES('tenant','tenant-a','credential','resource-a','v4097','installation',0,0,1)").await?;
        let error = load(&db, &installation, 1.try_into()?)
            .await
            .err()
            .context("overflow accepted")?;
        ensure!(error.to_string() == "resource lifecycle capacity exceeded");
        ensure!(write(&db, &installation, &item).await.is_err());
        Ok(())
    }
}
